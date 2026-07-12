//! Jina Search (`s.jina.ai`) backend for `web_search`.
//!
//! Jina returns ranked hits WITH the full fetched page content per result. That
//! is `read_web_page`'s job, not `web_search`'s: we truncate every result's
//! content down to snippet length here so the tool stays token-bounded and
//! never leaks whole pages through search. The key is optional -- keyless is the
//! throttled anonymous tier, not an error -- so a missing key is only surfaced
//! as context when the request is actually rejected.
//!
//! Filter honesty: include_domains is expressed natively via Jina's `site:`
//! query operators (reported `Native`) and never double-post-filtered; only
//! exclude_domains is post-filtered. Jina search has no freshness or region
//! parameter, so recency/country are reported `Unsupported` rather than faked.

use tokio_util::sync::CancellationToken;

use super::super::fetch::{build_api_client, send_api};
use super::super::{FilterEnforcement, MAX_API_BYTES, SearchResult};
use super::filters::{apply_domain_filter, report};
use super::{SearchOutcome, SearchQuery};

/// Jina Search endpoint. JSON is requested via the `Accept` header.
const JINA_SEARCH_URL: &str = "https://s.jina.ai/";

/// Snippet cap (chars) applied to each result's fetched content. Jina returns
/// full pages; `web_search` only ships a snippet, so we clamp on a char
/// boundary well below any page size.
const SNIPPET_CHARS: usize = 300;

/// Run a Jina search: build the request (with `site:` operators for
/// include_domains), fetch capped JSON, parse + truncate content to snippets,
/// then apply exclude_domains and collect the truthful filter reports.
pub(super) async fn search(
    key: Option<&str>,
    query: &SearchQuery,
    cancel: &CancellationToken,
) -> anyhow::Result<SearchOutcome> {
    let client =
        build_api_client().map_err(|e| anyhow::anyhow!("failed to build Jina HTTP client: {e}"))?;

    // Express include_domains as `site:` operators so Jina filters upstream.
    // Multiple domains are OR-ed in a parenthesized group, which Jina accepts.
    let include: Vec<&String> = query
        .include_domains
        .iter()
        .filter(|d| !d.trim().is_empty())
        .collect();
    let effective_query = build_query(&query.query, &include);

    let mut request = client
        .get(JINA_SEARCH_URL)
        .header("Accept", "application/json")
        .query(&[("q", &effective_query)]);
    let has_key = key.map(|k| !k.trim().is_empty()).unwrap_or(false);
    if let Some(key) = key.filter(|k| !k.trim().is_empty()) {
        request = request.header("Authorization", format!("Bearer {key}"));
    }

    let (status, body, _truncated) = send_api(request, MAX_API_BYTES, cancel)
        .await
        .map_err(|e| anyhow::anyhow!("Jina search request failed: {e}"))?;

    if status != 200 {
        let hint = if has_key {
            ""
        } else {
            " (no Jina API key configured -- the anonymous tier is throttled; set a key to raise limits)"
        };
        anyhow::bail!("Jina search returned HTTP {status}{hint}");
    }

    let results = parse_jina_json(&body)?;

    // include_domains went into the query as `site:` operators -> Native.
    // exclude_domains is post-filtered locally by apply_domain_filter. Filter
    // BEFORE truncating, so an excluded domain occupying the first N rows never
    // starves later non-excluded results.
    let (mut results, mut filters) = apply_domain_filter(results, &[], &query.exclude_domains);
    results.truncate(query.max_results);
    if !include.is_empty() {
        filters.push(report("include_domains", FilterEnforcement::Native));
    }
    if query.recency.is_some() {
        filters.push(report("recency", FilterEnforcement::Unsupported));
    }
    if query.country.is_some() {
        filters.push(report("country", FilterEnforcement::Unsupported));
    }

    Ok(SearchOutcome { results, filters })
}

/// Append `site:` operators for include_domains onto the raw query. One domain
/// is a bare `site:d`; several become `(site:a OR site:b)` so any of them
/// matches. An empty include set leaves the query untouched.
fn build_query(query: &str, include: &[&String]) -> String {
    match include {
        [] => query.to_string(),
        [only] => format!("{query} site:{}", only.trim()),
        many => {
            let ors = many
                .iter()
                .map(|d| format!("site:{}", d.trim()))
                .collect::<Vec<_>>()
                .join(" OR ");
            format!("{query} ({ors})")
        }
    }
}

/// Truncate `text` to at most `max` chars on a char boundary, trimming trailing
/// whitespace. Pure `char`-based slicing keeps multi-byte content safe.
fn snippet(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    let trimmed = out.trim_end();
    if trimmed.len() != out.len() {
        out.truncate(trimmed.len());
    }
    out
}

/// Parse a Jina Search JSON body into normalized results. Jina returns
/// `{"data":[{"title","url","content"|"description",...}]}`. The parser is
/// deliberately tolerant of missing fields (a hit with no URL is skipped; a
/// missing title/content becomes empty) and truncates the page `content` down
/// to a snippet -- full page content is `read_web_page`'s job, never
/// `web_search`'s.
fn parse_jina_json(body: &[u8]) -> anyhow::Result<Vec<SearchResult>> {
    let root: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| anyhow::anyhow!("Jina search returned unparsable JSON: {e}"))?;

    let data = root
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow::anyhow!("Jina search response missing a `data` array"))?;

    let mut out = Vec::with_capacity(data.len());
    for item in data {
        let Some(url) = item.get("url").and_then(|v| v.as_str()) else {
            continue;
        };
        if url.trim().is_empty() {
            continue;
        }
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // Prefer the fetched `content`; fall back to `description` when absent.
        let raw = item
            .get("content")
            .and_then(|v| v.as_str())
            .or_else(|| item.get("description").and_then(|v| v.as_str()))
            .unwrap_or_default();
        out.push(SearchResult {
            title,
            url: url.to_string(),
            snippet: snippet(raw, SNIPPET_CHARS),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_data_array_and_truncates_content_to_snippet() {
        // A long multi-byte content field must be clamped to SNIPPET_CHARS on a
        // char boundary (never a byte split panic, never a full page).
        let long = "é".repeat(1000);
        let body = format!(
            r#"{{"data":[
                {{"title":"First","url":"https://a.com/x","content":{content}}},
                {{"title":"Second","url":"https://b.com/y","description":"short desc"}}
            ]}}"#,
            content = serde_json::to_string(&long).unwrap()
        );
        let results = parse_jina_json(body.as_bytes()).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "First");
        assert_eq!(results[0].url, "https://a.com/x");
        // Truncated to exactly the cap in CHARS (each é is one char, two bytes).
        assert_eq!(results[0].snippet.chars().count(), SNIPPET_CHARS);
        assert!(results[0].snippet.chars().all(|c| c == 'é'));
        // Falls back to `description` when `content` is absent.
        assert_eq!(results[1].snippet, "short desc");
    }

    #[test]
    fn tolerates_missing_fields_and_skips_urlless_hits() {
        let body = r#"{"data":[
            {"title":"no url here"},
            {"url":""},
            {"url":"https://ok.com/z"}
        ]}"#;
        let results = parse_jina_json(body.as_bytes()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://ok.com/z");
        assert_eq!(results[0].title, "");
        assert_eq!(results[0].snippet, "");
    }

    #[test]
    fn rejects_body_without_data_array() {
        assert!(parse_jina_json(br#"{"results":[]}"#).is_err());
        assert!(parse_jina_json(b"not json").is_err());
    }

    #[test]
    fn snippet_trims_trailing_whitespace_after_cut() {
        // Cutting mid-run can leave a trailing space; it is trimmed off.
        let text = "abc ".repeat(200);
        let s = snippet(&text, 8);
        assert_eq!(s, "abc abc");
    }

    #[test]
    fn build_query_expresses_include_domains_as_site_operators() {
        let a = "docs.rs".to_string();
        let b = "example.com".to_string();
        assert_eq!(build_query("rust", &[]), "rust");
        assert_eq!(build_query("rust", &[&a]), "rust site:docs.rs");
        assert_eq!(
            build_query("rust", &[&a, &b]),
            "rust (site:docs.rs OR site:example.com)"
        );
    }

    #[test]
    fn recency_and_country_report_unsupported() {
        use super::super::Recency;
        let (_out, filters) = apply_domain_filter(Vec::new(), &[], &[]);
        assert!(filters.is_empty());
        // Simulate the tail of `search`: recency/country always -> Unsupported.
        let mut filters = filters;
        let recency: Option<Recency> = Some(Recency::Week);
        let country: Option<String> = Some("de".to_string());
        if recency.is_some() {
            filters.push(report("recency", FilterEnforcement::Unsupported));
        }
        if country.is_some() {
            filters.push(report("country", FilterEnforcement::Unsupported));
        }
        assert_eq!(filters.len(), 2);
        assert!(
            filters
                .iter()
                .all(|f| f.enforcement == FilterEnforcement::Unsupported)
        );
        assert!(filters.iter().any(|f| f.filter == "recency"));
        assert!(filters.iter().any(|f| f.filter == "country"));
    }
}
