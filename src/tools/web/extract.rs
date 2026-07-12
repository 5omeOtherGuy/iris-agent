//! HTML -> Markdown article extraction for the native `read_web_page` reader.
//!
//! Pipeline (cheap-first, mirroring the reference TS reader):
//!
//!   fetched HTML ──▶ dom_smoothie (readability.js port) ──▶ article node/title
//!                                     │
//!                                     ▼
//!                         htmd (HTML -> Markdown, tables included)
//!                                     │
//!                                     ▼
//!                 readable-content assessment / char-budget truncation
//!
//! This stage is pure and synchronous on purpose: the caller runs it inside
//! `spawn_blocking`, and fetching plus SSRF policy live elsewhere. It never
//! panics on malformed HTML -- dom_smoothie failures and empty grabs fall back
//! to a zero-dependency plain-text strip (via `dom_query`) so we always return
//! *something* truthful, and a genuine JS shell / cookie-banner soup is
//! reported honestly as `readable = false` rather than dressed up as an article.

use dom_query::Document;
use dom_smoothie::{Config, Readability, TextMode};

/// Minimum Markdown length (chars) dom_smoothie must yield before we trust its
/// article grab. Below this, link directories, login walls, and error pages
/// masquerade as articles, so we drop to the fallback or the honest diagnostic.
const ARTICLE_MIN_CHARS: usize = 80;

/// Minimum plain-text length (chars) the zero-dep fallback must produce before
/// we treat it as meaningful readable content. A JS application shell strips to
/// near-nothing once scripts are removed, so it lands below this bar.
const FALLBACK_MIN_CHARS: usize = 200;

/// dom_smoothie's grab needs a lower character threshold than its 500-char
/// default to accept short-but-real articles; this mirrors the reference
/// reader's `charThreshold: 200`.
const GRAB_CHAR_THRESHOLD: usize = 200;

/// Honest content returned when nothing readable could be extracted. The caller
/// decides whether to surface this or point the user at the Jina reader.
const NO_CONTENT_DIAGNOSTIC: &str = "No readable article content could be extracted from this page. It may be a \
     JavaScript application shell, a consent/cookie wall, or otherwise empty.";

/// Honest diagnostic when the page looks like a static snapshot of a
/// JavaScript single-page app: an empty mount point with no server-rendered
/// prose. The native reader only sees static HTML, so it says so plainly; the
/// caller points at the JS-rendering Jina backend as the next step.
const SPA_DIAGNOSTIC: &str = "This page appears to be a JavaScript single-page application: the static \
     HTML is an empty app shell with no server-rendered article text, so there \
     is nothing for the native reader to extract.";

/// Non-content "chrome" landmarks removed before extraction so navigation,
/// banners, sidebars, and forms cannot masquerade as article prose (or inflate
/// a JS shell past the readable-content bar). Applied to BOTH the readability
/// grab and the plain-text fallback.
const CHROME_SELECTOR: &str = "script, style, noscript, template, svg, iframe, \
     nav, header, footer, aside, form, \
     [role=\"navigation\"], [role=\"banner\"], [role=\"contentinfo\"], \
     [role=\"dialog\"], [aria-modal=\"true\"]";

/// Cookie/consent banner containers removed before extraction. Targeted hooks
/// (specific SDK ids/classes and `consent`/`cookie-consent`/`gdpr` substrings)
/// rather than a broad `cookie` match, to avoid deleting a legitimate article.
const CONSENT_SELECTOR: &str = "#onetrust-banner-sdk, #onetrust-consent-sdk, .cookie-banner, \
     .cookie-consent, .cookie-notice, .consent-banner, .cc-window, \
     [class*=\"cookie-consent\"], [class*=\"consent-banner\"], \
     [id*=\"cookie-consent\"], [id*=\"cookie-banner\"], [id*=\"cookie-notice\"], \
     [id*=\"consent-banner\"], [id*=\"gdpr\"], [class*=\"gdpr\"]";

/// Result of the local extraction stage. Consumed by `read/native.rs`.
pub(super) struct Extraction {
    /// Extracted Markdown (or plain-text fallback, or a short honest
    /// diagnostic when not readable).
    pub(super) content: String,
    /// Document title when available.
    pub(super) title: Option<String>,
    /// Whether the pipeline produced meaningful readable content.
    pub(super) readable: bool,
    /// Whether `content` was truncated by `max_chars`.
    pub(super) truncated: bool,
}

/// Extract readable Markdown from `html`. `base_url` resolves relative links
/// where dom_smoothie supports it (only when it is an absolute URL). `max_chars`
/// bounds the returned content. Pure/sync -- the caller runs it in
/// `spawn_blocking`. Never panics on malformed HTML: it always returns an
/// [`Extraction`].
pub(super) fn extract_markdown(html: &str, base_url: &str, max_chars: usize) -> Extraction {
    // Strip navigation/consent chrome once, up front, so neither the
    // readability grab nor the fallback mistakes it for article content.
    let cleaned = strip_chrome(html);

    // Preferred path: dom_smoothie article grab -> htmd Markdown.
    if let Some((markdown, title)) = readability_markdown(&cleaned, base_url)
        && markdown.chars().count() >= ARTICLE_MIN_CHARS
    {
        let (content, truncated) = truncate_chars(markdown, max_chars);
        return Extraction {
            content,
            title,
            readable: true,
            truncated,
        };
    }

    // Fallback: strip tags to plain text so we still return something truthful.
    // Title comes from the ORIGINAL document (chrome removal leaves <head>).
    let fallback_title = document_title(html);
    let text = fallback_text(&cleaned);
    if text.chars().count() >= FALLBACK_MIN_CHARS {
        let (content, truncated) = truncate_chars(text, max_chars);
        return Extraction {
            content,
            title: fallback_title,
            readable: true,
            truncated,
        };
    }

    // Nothing usable: report honestly instead of emitting empty or fake content.
    // Distinguish a JS single-page-app shell from a generic empty page so the
    // diagnostic is specific and the Jina (JS-rendering) pointer is warranted.
    let content = if looks_like_spa_shell(html) {
        SPA_DIAGNOSTIC
    } else {
        NO_CONTENT_DIAGNOSTIC
    };
    Extraction {
        content: content.to_string(),
        title: fallback_title,
        readable: false,
        truncated: false,
    }
}

/// Remove navigation/consent chrome from `html` and return the re-serialized
/// document. Idempotent and panic-free on malformed HTML (dom_query is
/// lenient); the selectors are constant and validated by the unit tests.
fn strip_chrome(html: &str) -> String {
    let doc = Document::from(html);
    doc.select(CHROME_SELECTOR).remove();
    doc.select(CONSENT_SELECTOR).remove();
    doc.html().to_string()
}

/// Heuristic: does the ORIGINAL HTML look like a static JavaScript SPA shell?
/// True when the parsed DOM exposes a common framework mount point
/// (`#root`/`#app`/`#__next`/React root/Angular app) or the markup carries an
/// "enable JavaScript" placeholder. Uses the DOM selector engine (not raw
/// string matching) so attribute-whitespace/quote variants are handled. Only
/// consulted once we already know no readable content was extracted, so a false
/// positive only swaps one honest diagnostic for a more specific one.
fn looks_like_spa_shell(html: &str) -> bool {
    let has_mount = Document::from(html)
        .select("#root, #app, #__next, [data-reactroot], [ng-app], [ng-version]")
        .exists();
    let lower = html.to_ascii_lowercase();
    let mentions_js = lower.contains("enable javascript")
        || lower.contains("javascript is required")
        || lower.contains("please enable js");
    has_mount || mentions_js
}

/// Run dom_smoothie then htmd. Returns `(markdown, title)` on a successful grab
/// with non-empty content, or `None` when the parser errors or grabs nothing --
/// dom_smoothie returns `Err(GrabFailed)` on thin documents, which is expected,
/// not exceptional, so we swallow it and let the caller fall back.
fn readability_markdown(html: &str, base_url: &str) -> Option<(String, Option<String>)> {
    let mut cfg = Config {
        char_threshold: GRAB_CHAR_THRESHOLD,
        ..Config::default()
    };
    // Raw HTML content is what htmd needs; formatted/markdown text modes would
    // pre-mangle the node before we convert it ourselves.
    cfg.text_mode = TextMode::Raw;

    // dom_smoothie rejects a relative document URL up front, so only pass one
    // when it is absolute; otherwise extraction still works, just without
    // relative-link resolution.
    let doc_url = if is_absolute_url(base_url) {
        Some(base_url)
    } else {
        None
    };

    let mut readability = Readability::new(html, doc_url, Some(cfg)).ok()?;
    let article = readability.parse().ok()?;

    let content_html = article.content.trim();
    if content_html.is_empty() {
        return None;
    }

    let markdown = htmd::convert(content_html).ok()?;
    let markdown = markdown.trim();
    if markdown.is_empty() {
        return None;
    }

    let title = normalize_title(&article.title);
    // Prepend the title as an H1 when dom_smoothie surfaced one and the grabbed
    // Markdown does not already open with a heading, so the article keeps its
    // headline in the rendered output.
    let markdown = match &title {
        Some(t) if !markdown.starts_with('#') => format!("# {t}\n\n{markdown}"),
        _ => markdown.to_string(),
    };

    Some((markdown, title))
}

/// Zero-dependency last resort: take the (already chrome-stripped) document's
/// formatted text. Re-strips script/style defensively in case a caller passes
/// un-cleaned HTML, so an inline `<script>` body can never inflate the fallback
/// and mask a JS shell as readable.
fn fallback_text(html: &str) -> String {
    let doc = Document::from(html);
    doc.select("script, style, noscript, template, svg, iframe")
        .remove();
    doc.formatted_text().trim().to_string()
}

/// Best-effort `<title>` for the fallback / diagnostic paths, where
/// dom_smoothie did not run or produced nothing.
fn document_title(html: &str) -> Option<String> {
    let doc = Document::from(html);
    normalize_title(&doc.select("title").text())
}

/// Collapse whitespace and reject an empty title.
fn normalize_title(raw: &str) -> Option<String> {
    let title = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() { None } else { Some(title) }
}

/// True for `http`/`https` absolute URLs, the only base URLs dom_smoothie
/// accepts. A stricter parse is unnecessary here: an invalid absolute URL just
/// disables relative-link resolution, it does not break extraction.
fn is_absolute_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Truncate to `max_chars` on a UTF-8 char boundary (never splitting a
/// codepoint), reporting whether a cut happened.
fn truncate_chars(mut text: String, max_chars: usize) -> (String, bool) {
    // char_indices gives byte offsets at codepoint boundaries; the offset of the
    // (max_chars)-th char is a safe truncation point.
    if let Some((byte_idx, _)) = text.char_indices().nth(max_chars) {
        text.truncate(byte_idx);
        (text, true)
    } else {
        (text, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTICLE_HTML: &str = r#"
        <html><head><title>  Widget   Guide </title></head><body>
        <nav>home about contact</nav>
        <article>
          <h1>How Widgets Work</h1>
          <p>Widgets are small composable parts that snap together to build
             larger systems. This guide walks through the core ideas and shows
             why they matter for everyday engineering work.</p>
          <p>The second paragraph expands on assembly, maintenance, and the
             trade-offs involved when you compose many widgets into one device
             that has to keep running reliably for a long time.</p>
        </article>
        <footer>copyright</footer>
        </body></html>
    "#;

    #[test]
    fn simple_article_extracts_markdown_and_title() {
        let out = extract_markdown(ARTICLE_HTML, "https://example.com/guide", 5000);
        assert!(out.readable, "article should be readable");
        assert_eq!(out.title.as_deref(), Some("Widget Guide"));
        assert!(
            out.content.contains("How Widgets Work"),
            "heading text missing: {}",
            out.content
        );
        assert!(
            out.content.contains("Widgets are small composable parts"),
            "paragraph text missing: {}",
            out.content
        );
        assert!(!out.truncated);
    }

    #[test]
    fn heading_and_paragraph_both_present() {
        let out = extract_markdown(ARTICLE_HTML, "https://example.com/guide", 5000);
        assert!(out.content.contains("How Widgets Work"));
        assert!(out.content.contains("keep running reliably"));
    }

    #[test]
    fn table_renders_as_markdown() {
        let html = r#"
            <html><head><title>Data Report</title></head><body>
            <article>
              <h1>Quarterly Data Report</h1>
              <p>The table below summarizes the measured throughput for each
                 region across the last reporting period, along with the notes
                 our analysts attached to every row during the review.</p>
              <table>
                <thead><tr><th>Region</th><th>Score</th></tr></thead>
                <tbody>
                  <tr><td>North</td><td>42</td></tr>
                  <tr><td>South</td><td>37</td></tr>
                </tbody>
              </table>
              <p>These figures feed the capacity plan and are revisited whenever
                 the underlying demand assumptions shift by a meaningful amount.</p>
            </article>
            </body></html>
        "#;
        let out = extract_markdown(html, "https://example.com/report", 5000);
        assert!(out.readable, "content: {}", out.content);
        assert!(
            out.content.contains('|') && out.content.contains("Region"),
            "expected a Markdown table, got: {}",
            out.content
        );
    }

    #[test]
    fn js_shell_is_reported_as_spa() {
        let html = r#"
            <html><head><title>App</title></head><body>
            <div id="root"></div>
            <script>window.__DATA__ = {a:1,b:2,c:3}; boot();</script>
            </body></html>
        "#;
        let out = extract_markdown(html, "https://example.com/", 5000);
        assert!(!out.readable, "JS shell should not be readable");
        assert_eq!(out.content, SPA_DIAGNOSTIC);
    }

    // ------------------------------------------------------------------
    // Fixture-driven cases (real HTML files under testdata/).
    // ------------------------------------------------------------------

    const FIXTURE_TABLE: &str = include_str!("testdata/article_table.html");
    const FIXTURE_CODE: &str = include_str!("testdata/article_code.html");
    const FIXTURE_SPA: &str = include_str!("testdata/spa_shell.html");
    const FIXTURE_CONSENT: &str = include_str!("testdata/consent_wall.html");

    #[test]
    fn fixture_article_renders_table_and_drops_chrome() {
        let out = extract_markdown(FIXTURE_TABLE, "https://example.com/report", 5000);
        assert!(out.readable, "content: {}", out.content);
        assert_eq!(out.title.as_deref(), Some("Quarterly Data Report"));
        // Table survives as Markdown.
        assert!(
            out.content.contains('|') && out.content.contains("Region"),
            "expected a Markdown table, got: {}",
            out.content
        );
        assert!(out.content.contains("North") && out.content.contains("East"));
        // Navigation / promo / footer chrome is gone.
        assert!(
            !out.content.contains("login signup"),
            "nav leaked: {}",
            out.content
        );
        assert!(
            !out.content.contains("Subscribe to our newsletter"),
            "aside leaked: {}",
            out.content
        );
        assert!(
            !out.content.contains("All rights reserved"),
            "footer leaked: {}",
            out.content
        );
    }

    #[test]
    fn fixture_article_preserves_code_block() {
        let out = extract_markdown(FIXTURE_CODE, "https://example.com/rust", 5000);
        assert!(out.readable, "content: {}", out.content);
        assert!(
            out.content.contains("fs::read_to_string"),
            "code body missing: {}",
            out.content
        );
        assert!(
            out.content.contains("fn main"),
            "code body missing: {}",
            out.content
        );
    }

    #[test]
    fn fixture_spa_shell_is_reported_as_spa() {
        let out = extract_markdown(FIXTURE_SPA, "https://example.com/dashboard", 5000);
        assert!(!out.readable);
        assert_eq!(out.content, SPA_DIAGNOSTIC);
    }

    #[test]
    fn id_only_consent_wall_is_stripped() {
        // A consent banner hooked only by `id` (no matching class) must still be
        // removed so it cannot masquerade as readable article prose.
        let html = r#"
            <html><head><title>Story</title></head><body>
            <div id="cookie-consent-root">
              <p>We and our partners use cookies and process personal data such as
                 unique identifiers to personalise content and measure performance,
                 and you may accept or reject these purposes at any time via the
                 privacy settings link shown at the bottom of every single page.</p>
            </div>
            <main><article><p>Loading story…</p></article></main>
            </body></html>
        "#;
        let out = extract_markdown(html, "https://example.com/story", 5000);
        assert!(!out.readable, "content: {}", out.content);
        assert!(
            !out.content.contains("process personal data"),
            "id-only consent banner leaked: {}",
            out.content
        );
    }

    #[test]
    fn fixture_consent_wall_is_not_readable() {
        let out = extract_markdown(FIXTURE_CONSENT, "https://example.com/news", 5000);
        assert!(!out.readable, "consent wall must not read as an article");
        // The cookie-banner prose must not survive as "content".
        assert!(
            !out.content.contains("We value your privacy"),
            "consent banner leaked: {}",
            out.content
        );
        assert!(
            !out.content.contains("842 partners"),
            "consent banner leaked: {}",
            out.content
        );
    }

    #[test]
    fn malformed_html_does_not_panic() {
        let html = "<html><body><p>unclosed <b>bold <div>weird <<<>>";
        let out = extract_markdown(html, "not a url", 5000);
        // The assertion is simply that we returned without panicking.
        let _ = out.readable;
    }

    #[test]
    fn max_chars_truncates_on_char_boundary() {
        // Multibyte content so a naive byte cut would split a codepoint.
        let body = "café ".repeat(200);
        let html = format!(
            "<html><head><title>T</title></head><body><article><h1>Heading Here</h1><p>{body}</p></article></body></html>"
        );
        let out = extract_markdown(&html, "https://example.com/", 50);
        assert!(out.truncated, "content should be truncated");
        assert_eq!(out.content.chars().count(), 50);
        // Valid UTF-8 by construction (String), and no replacement char left.
        assert!(!out.content.contains('\u{fffd}'));
    }

    #[test]
    fn empty_input_reports_no_content() {
        let out = extract_markdown("", "https://example.com/", 5000);
        assert!(!out.readable);
        assert_eq!(out.content, NO_CONTENT_DIAGNOSTIC);
    }
}
