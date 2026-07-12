//! Native pinned-fetch reader: SSRF-safe fetch (see [`fetch`](super::super::fetch))
//! followed by local HTML -> Markdown extraction
//! (see [`extract`](super::super::extract)).
//!
//! The split of concerns here is deliberate:
//! - Genuine fetch failures (policy denial, DNS, transport, timeout,
//!   cancellation, undecodable encoding, redirect loop) surface as
//!   `anyhow::Error` -- the model cannot recover from them by reading further.
//! - An *unsupported content type* (PDF, image, octet-stream) is NOT an error:
//!   the native reader simply cannot parse it, but the Jina backend can. We
//!   return an honest diagnostic in [`PageResult::content`] naming that
//!   alternative, so the model sees actionable guidance instead of a dead end.
//! - A fetched-but-unreadable HTML page (JS application shell, cookie wall)
//!   likewise returns Ok with the extractor's honest diagnostic plus a pointer
//!   at Jina, since Jina renders JavaScript.
//!
//! Extraction is CPU-bound (readability parse + Markdown conversion), so it runs
//! inside `spawn_blocking` to keep the async runtime responsive. Objective
//! excerpting is applied by the caller after we return -- this returns the full
//! extracted content.

use tokio_util::sync::CancellationToken;

use super::super::extract::{Extraction, extract_markdown};
use super::super::fetch::{FetchError, FetchedPage, Resolver, fetch_pinned};
use super::super::{MAX_BODY_BYTES, PageResult};
use super::ReadRequest;

/// Fetch `request.url` through the pinned SSRF-safe path, then extract locally.
/// Returns Ok for both readable pages and honest "cannot read this here"
/// diagnostics (unsupported content types, JS shells); only genuine fetch
/// failures bail. Never panics.
pub(super) async fn read(
    request: &ReadRequest,
    resolver: &dyn Resolver,
    cancel: &CancellationToken,
) -> anyhow::Result<PageResult> {
    let page = match fetch_pinned(resolver, &request.url, cancel).await {
        Ok(page) => page,
        // A content type we cannot parse is guidance, not a hard error: point
        // the model at the Jina backend rather than failing the read.
        Err(FetchError::UnsupportedContentType(ct)) => {
            return Ok(unsupported_content_page(&request.url, &ct));
        }
        // Every other variant is a real failure the model cannot read past.
        // Include the underlying Display so the message is actionable.
        Err(err) => anyhow::bail!("failed to read {}: {err}", request.url),
    };

    // Passthrough content (plain text, JSON, markdown) ships verbatim -- no
    // HTML extraction (and no title). Only markup is extracted below.
    if page.passthrough {
        return Ok(page_from_fetched(page, None));
    }

    // Markup: run the CPU-bound extraction off the async runtime. Clone the
    // owned Strings the blocking task needs so it borrows nothing from `page`.
    let html = page.text.clone();
    let base_url = page.final_url.clone();
    let extraction =
        tokio::task::spawn_blocking(move || extract_markdown(&html, &base_url, MAX_BODY_BYTES))
            .await
            .map_err(|e| anyhow::anyhow!("extraction task failed: {e}"))?;

    Ok(page_from_fetched(page, Some(extraction)))
}

/// Honest diagnostic for a content type the native reader cannot parse. Names a
/// concrete example (PDF/binary) and the Jina backend as the way to read it.
fn diagnostic_for_unsupported(content_type: &str) -> String {
    format!(
        "This resource is `{content_type}` (e.g. a PDF or binary). The native \
         reader only handles HTML and text; switch read_web_page to the `jina` \
         backend to read it."
    )
}

/// Sentence appended when HTML fetched fine but yielded no readable content.
/// The extractor already explains *why* (JS shell / consent wall); this adds
/// the actionable next step, since Jina renders JavaScript.
const JS_SHELL_JINA_HINT: &str = " If this is a JavaScript-rendered page, switch read_web_page to the \
     `jina` backend, which renders JavaScript before extracting.";

/// Build the diagnostic [`PageResult`] for an unsupported content type. We have
/// no real status/redirect data here (the fetch layer refused before reading a
/// body), so status is a `0` sentinel and the URL is echoed back unchanged.
fn unsupported_content_page(url: &str, content_type: &str) -> PageResult {
    PageResult {
        content: diagnostic_for_unsupported(content_type),
        final_url: url.to_string(),
        status: 0,
        title: None,
        truncated: false,
        redirects: 0,
    }
}

/// Assemble the final [`PageResult`] from a fetched page and optional
/// extraction. `extraction == None` is the plain-text passthrough (verbatim
/// body, no title). `Some(readable)` uses the extracted Markdown + title;
/// `Some(unreadable)` keeps the extractor's honest diagnostic but appends the
/// Jina pointer so the model still gets a next step. Pure and testable.
fn page_from_fetched(page: FetchedPage, extraction: Option<Extraction>) -> PageResult {
    let FetchedPage {
        final_url,
        status,
        text,
        truncated,
        redirects,
        ..
    } = page;

    let (content, title, truncated) = match extraction {
        // Plain-text passthrough: body verbatim, title unknown.
        None => (text, None, truncated),
        Some(ex) if ex.readable => (ex.content, ex.title, truncated || ex.truncated),
        // Fetched but not readable: honest diagnostic + Jina pointer.
        Some(ex) => {
            let content = format!("{}{JS_SHELL_JINA_HINT}", ex.content);
            (content, ex.title, truncated || ex.truncated)
        }
    };

    PageResult {
        content,
        final_url,
        status,
        title,
        truncated,
        redirects,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a fetched page for the pure-helper tests. `fetch_pinned` itself
    /// is never called here -- it needs the network and the SSRF gate denies the
    /// loopback a local server would use.
    fn fetched(passthrough: bool, text: &str, truncated: bool) -> FetchedPage {
        FetchedPage {
            final_url: "https://example.com/page".to_string(),
            status: 200,
            passthrough,
            text: text.to_string(),
            truncated,
            redirects: 1,
        }
    }

    fn extraction(
        content: &str,
        title: Option<&str>,
        readable: bool,
        truncated: bool,
    ) -> Extraction {
        Extraction {
            content: content.to_string(),
            title: title.map(str::to_string),
            readable,
            truncated,
        }
    }

    #[test]
    fn plain_text_passes_through_verbatim() {
        let raw = "line one\n  indented\nline three";
        let out = page_from_fetched(fetched(true, raw, false), None);
        assert_eq!(out.content, raw, "plain text must be verbatim");
        assert_eq!(out.title, None);
        assert!(!out.truncated);
        assert_eq!(out.status, 200);
        assert_eq!(out.final_url, "https://example.com/page");
        assert_eq!(out.redirects, 1);
    }

    #[test]
    fn plain_text_propagates_body_truncation() {
        let out = page_from_fetched(fetched(true, "capped body", true), None);
        assert!(out.truncated, "body cap must carry through");
    }

    #[test]
    fn readable_markdown_uses_extraction_content_and_title() {
        let ex = extraction("# Heading\n\nBody text.", Some("Heading"), true, false);
        let out = page_from_fetched(fetched(false, "<html>...</html>", false), Some(ex));
        assert_eq!(out.content, "# Heading\n\nBody text.");
        assert_eq!(out.title.as_deref(), Some("Heading"));
        assert!(!out.truncated);
    }

    #[test]
    fn readable_markdown_ors_truncation_flags() {
        // Extraction truncated even though the body was not -> result truncated.
        let ex = extraction("cut markdown", Some("T"), true, true);
        let out = page_from_fetched(fetched(false, "<html>", false), Some(ex));
        assert!(out.truncated);
    }

    #[test]
    fn non_readable_page_appends_jina_hint() {
        let ex = extraction(
            "No readable article content could be extracted from this page.",
            Some("App"),
            false,
            false,
        );
        let out = page_from_fetched(fetched(false, "<div id=root></div>", false), Some(ex));
        assert!(
            out.content.starts_with("No readable article content"),
            "diagnostic kept: {}",
            out.content
        );
        assert!(
            out.content.contains("jina"),
            "jina pointer appended: {}",
            out.content
        );
        // Still returns the extracted title so the model can identify the page.
        assert_eq!(out.title.as_deref(), Some("App"));
    }

    #[test]
    fn unsupported_content_diagnostic_names_jina_and_pdf() {
        let out = unsupported_content_page("https://example.com/doc.pdf", "application/pdf");
        assert!(out.content.contains("application/pdf"));
        assert!(out.content.contains("PDF"));
        assert!(out.content.contains("jina"));
        // Sentinel status + echoed URL, no body data available at this stage.
        assert_eq!(out.status, 0);
        assert_eq!(out.final_url, "https://example.com/doc.pdf");
        assert_eq!(out.title, None);
        assert!(!out.truncated);
        assert_eq!(out.redirects, 0);
    }

    #[test]
    fn diagnostic_for_unsupported_echoes_the_content_type() {
        let msg = diagnostic_for_unsupported("image/png");
        assert!(msg.contains("`image/png`"));
        assert!(msg.contains("jina"));
    }
}
