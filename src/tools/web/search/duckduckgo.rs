//! DuckDuckGo native (keyless) web-search backend.
//!
//! Scrapes the public `https://html.duckduckgo.com/html/` endpoint and parses
//! the rendered result rows. No API key is required, so this is the default
//! no-key backend. When the HTML endpoint throttles (DDG answers a bot
//! challenge with HTTP 202 + a "Ratelimit"/anomaly body) we retry the
//! `https://lite.duckduckgo.com/lite/` endpoint, whose lighter markup is a
//! useful drift fallback; if that also throttles we fail with an actionable
//! error naming the brave/jina backends instead of returning a silent empty.
//!
//! Best-effort resource bounds, since DDG punishes scraping:
//! - a bounded per-process query cache (TTL + LRU-ish, capped entries) so
//!   repeated identical searches do not re-hit DDG, and
//! - a per-process backoff window opened on a detected throttle, so we fail
//!   fast during the cool-down rather than issuing more blocked requests.
//!
//! Result hrefs are wrapped in a `/l/?uddg=<encoded>` click-tracker redirect;
//! we decode the canonical target locally (never following the tracker), so the
//! model receives the real URL and DDG's click counter is never exercised.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use dom_query::Document;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::super::{FilterEnforcement, SearchResult};
use super::filters::{apply_domain_filter, report};
use super::{SearchOutcome, SearchQuery};

/// Primary endpoint: the rendered HTML result page.
const HTML_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
/// Fallback endpoint: the lighter "lite" table markup, used when the HTML
/// endpoint throttles or drifts.
const LITE_ENDPOINT: &str = "https://lite.duckduckgo.com/lite/";

/// Cached entries live this long before a re-fetch.
const CACHE_TTL: Duration = Duration::from_secs(120);
/// Cap on cached queries (LRU-ish eviction from the front).
const CACHE_MAX: usize = 32;
/// Cool-down opened on a detected throttle; calls fail fast until it elapses.
const BACKOFF: Duration = Duration::from_secs(60);

/// Actionable hint appended to throttle/empty errors so the user knows the
/// keyed alternatives exist.
const THROTTLE_HINT: &str = "DuckDuckGo keyless search is rate-limited or blocked. \
     Use the brave (requires an API key) or jina backend for higher reliability.";

/// Per-process cache + backoff state. The web tools are exclusive (one call at a
/// time), so a plain `Mutex` guarded by a `OnceLock` is sufficient and simple;
/// the lock is never held across an `.await`.
struct CacheState {
    entries: VecDeque<CacheEntry>,
    blocked_until: Option<Instant>,
}

struct CacheEntry {
    key: String,
    expires: Instant,
    results: Vec<SearchResult>,
}

fn cache() -> &'static Mutex<CacheState> {
    static CACHE: OnceLock<Mutex<CacheState>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(CacheState {
            entries: VecDeque::new(),
            blocked_until: None,
        })
    })
}

/// One scrape attempt's shape: usable rows, a detected throttle, or a clean
/// fetch that yielded no parseable rows (markup drift or a soft block).
enum ScrapeOutcome {
    Results(Vec<SearchResult>),
    Throttled,
    Empty,
}

pub(super) async fn search(
    query: &SearchQuery,
    cancel: &CancellationToken,
) -> anyhow::Result<SearchOutcome> {
    let q = query.query.trim();
    if q.is_empty() {
        bail!("web_search requires a non-empty query (objective or search_queries[0]).");
    }
    // Normalize the cache key so trivially-different casings share an entry.
    let key = q.to_lowercase();

    let candidates = if let Some(cached) = cache_lookup(&key)? {
        cached
    } else {
        let results = fetch_candidates(q, cancel).await?;
        cache_store(key, &results);
        results
    };

    Ok(finalize(candidates, query))
}

/// Fetch the raw candidate rows: try the HTML endpoint, fall back to the lite
/// endpoint on throttle/empty, and fail with an actionable error (opening the
/// backoff window on a real throttle) when neither yields rows.
async fn fetch_candidates(
    q: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<Vec<SearchResult>> {
    let html = scrape(HTML_ENDPOINT, q, cancel, false).await?;
    match html {
        ScrapeOutcome::Results(rows) => Ok(rows),
        _ => {
            let lite = scrape(LITE_ENDPOINT, q, cancel, true).await?;
            match lite {
                ScrapeOutcome::Results(rows) => Ok(rows),
                lite_other => {
                    let throttled = matches!(html, ScrapeOutcome::Throttled)
                        || matches!(lite_other, ScrapeOutcome::Throttled);
                    if throttled {
                        open_backoff();
                        bail!("{THROTTLE_HINT}");
                    }
                    bail!(
                        "DuckDuckGo returned no parseable result rows (markup drift or a soft \
                         block). {THROTTLE_HINT}"
                    );
                }
            }
        }
    }
}

/// Fetch one endpoint through the pinned SSRF-safe path and classify the result.
async fn scrape(
    endpoint: &str,
    q: &str,
    cancel: &CancellationToken,
    lite: bool,
) -> anyhow::Result<ScrapeOutcome> {
    // `url` percent-encodes the query; a raw string concat could inject `&`/`#`.
    let url = Url::parse_with_params(endpoint, [("q", q)])
        .context("failed to build the DuckDuckGo query URL")?;

    let resolver = super::super::fetch::SystemResolver;
    let page = super::super::fetch::fetch_pinned(&resolver, url.as_str(), cancel)
        .await
        .map_err(|e| anyhow::anyhow!("DuckDuckGo fetch failed: {e}"))?;

    if is_throttled(page.status, &page.text) {
        return Ok(ScrapeOutcome::Throttled);
    }

    let results = if lite {
        parse_lite_results(&page.text)
    } else {
        parse_html_results(&page.text)
    };
    if results.is_empty() {
        Ok(ScrapeOutcome::Empty)
    } else {
        Ok(ScrapeOutcome::Results(results))
    }
}

/// Apply domain filters FIRST, then truncate to the requested count, and append
/// the truthful `unsupported` reports for filters DDG HTML cannot honor (it
/// exposes no reliable publication dates and this backend does not
/// region-target). Filtering before truncation is essential: post-filtered
/// domains ranked after the first N rows would otherwise be discarded before
/// they are ever considered. Pure, so the contract is unit-testable offline.
fn finalize(candidates: Vec<SearchResult>, query: &SearchQuery) -> SearchOutcome {
    let (mut results, mut filters) =
        apply_domain_filter(candidates, &query.include_domains, &query.exclude_domains);
    results.truncate(query.max_results);
    if query.recency.is_some() {
        filters.push(report("recency", FilterEnforcement::Unsupported));
    }
    if query.country.is_some() {
        filters.push(report("country", FilterEnforcement::Unsupported));
    }
    SearchOutcome { results, filters }
}

/// Detect a DDG throttle/bot challenge. DDG answers scraping with HTTP 202 and a
/// short anomaly/ratelimit body; we also sniff the head of a 200 body for the
/// same markers so a soft block is not mistaken for a genuinely empty result.
fn is_throttled(status: u16, body: &str) -> bool {
    if status == 202 {
        return true;
    }
    let head: String = body.chars().take(4096).collect::<String>().to_lowercase();
    const MARKERS: [&str; 6] = [
        "ratelimit",
        "rate limit",
        "anomaly",
        "unable to process your request",
        "verify you are a human",
        "captcha",
    ];
    MARKERS.iter().any(|m| head.contains(m))
}

/// Parse the HTML-endpoint result rows.
///
/// Structure (stable as of late 2025):
/// ```text
/// <div class="result results_links results_links_deep web-result">
///   <h2 class="result__title">
///     <a class="result__a" href="//duckduckgo.com/l/?uddg=...">Title</a>
///   </h2>
///   <a class="result__snippet" ...>Snippet</a>
/// </div>
/// ```
/// `dom_query` already yields entity-decoded text/attrs, so we only collapse
/// whitespace and decode the `uddg` click-tracker wrapper. Never panics on
/// malformed HTML: unmatched selectors simply yield empty selections.
fn parse_html_results(html: &str) -> Vec<SearchResult> {
    let doc = Document::from(html);
    let mut out = Vec::new();
    for row in doc.select("div.result").iter() {
        let anchor = row.select(".result__a");
        let Some(href) = anchor.attr("href") else {
            continue;
        };
        let Some(url) = decode_uddg(&href) else {
            continue;
        };
        let title = normalize_ws(&anchor.text());
        let snippet = normalize_ws(&row.select(".result__snippet").text());
        out.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    out
}

/// Parse the lite-endpoint result rows. The lite markup is a flat table: title
/// anchors carry `class="result-link"` and snippets live in a following
/// `td.result-snippet`, so we zip anchors to snippets positionally (best-effort;
/// a missing snippet just yields an empty string).
fn parse_lite_results(html: &str) -> Vec<SearchResult> {
    let doc = Document::from(html);
    let links: Vec<_> = doc.select("a.result-link").iter().collect();
    let snippets: Vec<_> = doc.select(".result-snippet").iter().collect();
    let mut out = Vec::new();
    for (i, link) in links.iter().enumerate() {
        let Some(href) = link.attr("href") else {
            continue;
        };
        let Some(url) = decode_uddg(&href) else {
            continue;
        };
        let title = normalize_ws(&link.text());
        let snippet = snippets
            .get(i)
            .map(|s| normalize_ws(&s.text()))
            .unwrap_or_default();
        out.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    out
}

/// Decode the canonical target out of DDG's `/l/?uddg=<encoded>` click-tracker
/// wrapper, returning `None` for anything that is not a usable http(s) URL. DDG
/// emits protocol-relative (`//duckduckgo.com/...`) and root-relative (`/l/...`)
/// hrefs, so we normalize to an absolute URL before parsing. `url`'s
/// `query_pairs` percent-decodes the `uddg` value exactly once. A non-tracker
/// absolute href (the lite endpoint sometimes links directly) is passed through.
fn decode_uddg(href: &str) -> Option<String> {
    let trimmed = href.trim();
    if trimmed.is_empty() {
        return None;
    }
    let absolute = if let Some(rest) = trimmed.strip_prefix("//") {
        format!("https://{rest}")
    } else if trimmed.starts_with('/') {
        format!("https://duckduckgo.com{trimmed}")
    } else {
        trimmed.to_string()
    };
    let parsed = Url::parse(&absolute).ok()?;

    let is_ddg = parsed.host_str().is_some_and(|h| {
        h.eq_ignore_ascii_case("duckduckgo.com") || h.ends_with(".duckduckgo.com")
    });
    let candidate = if is_ddg && parsed.path().starts_with("/l/") {
        parsed
            .query_pairs()
            .find(|(k, _)| k == "uddg")
            .map(|(_, v)| v.into_owned())?
    } else {
        absolute
    };

    if is_http_url(&candidate) {
        Some(candidate)
    } else {
        None
    }
}

/// Only accept http(s) targets; a decoded `javascript:`/`data:`/relative value
/// is not a usable web result.
fn is_http_url(u: &str) -> bool {
    u.starts_with("http://") || u.starts_with("https://")
}

/// Collapse runs of whitespace (including the newlines DOM text carries) to
/// single spaces and trim, so titles/snippets are compact single lines.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Read the cache under the backoff gate. Returns `Err` when a backoff window is
/// active (fail fast), `Ok(Some(_))` on a live hit (LRU-refreshed), else
/// `Ok(None)`. Expired entries are swept here.
fn cache_lookup(key: &str) -> anyhow::Result<Option<Vec<SearchResult>>> {
    let mut st = cache().lock().expect("web-tools cache mutex poisoned");
    let now = Instant::now();
    if let Some(until) = st.blocked_until {
        if now < until {
            let secs = until.saturating_duration_since(now).as_secs() + 1;
            bail!("{THROTTLE_HINT} (backoff active for ~{secs}s)");
        }
        st.blocked_until = None;
    }
    st.entries.retain(|e| e.expires > now);
    if let Some(pos) = st.entries.iter().position(|e| e.key == key) {
        let entry = st.entries.remove(pos).expect("position just found");
        let results = entry.results.clone();
        st.entries.push_back(entry);
        return Ok(Some(results));
    }
    Ok(None)
}

/// Store fresh results, replacing any stale entry for the same key and evicting
/// the oldest once over the cap.
fn cache_store(key: String, results: &[SearchResult]) {
    let mut st = cache().lock().expect("web-tools cache mutex poisoned");
    st.entries.retain(|e| e.key != key);
    st.entries.push_back(CacheEntry {
        key,
        expires: Instant::now() + CACHE_TTL,
        results: results.to_vec(),
    });
    while st.entries.len() > CACHE_MAX {
        st.entries.pop_front();
    }
}

/// Open the throttle cool-down so subsequent calls fail fast.
fn open_backoff() {
    let mut st = cache().lock().expect("web-tools cache mutex poisoned");
    st.blocked_until = Some(Instant::now() + BACKOFF);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(recency: Option<super::super::Recency>, country: Option<&str>) -> SearchQuery {
        SearchQuery {
            query: "rust async".into(),
            max_results: 10,
            include_domains: Vec::new(),
            exclude_domains: Vec::new(),
            recency,
            country: country.map(|c| c.to_string()),
        }
    }

    #[test]
    fn parse_html_decodes_uddg_and_strips_markup() {
        // Realistic DDG HTML row: protocol-relative click-tracker href with an
        // entity-encoded `&amp;` before `rut=`, plus a snippet anchor.
        let html = r#"
        <div class="result results_links results_links_deep web-result">
          <h2 class="result__title">
            <a class="result__a"
               href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=abc">
               Example <b>Title</b>
            </a>
          </h2>
          <a class="result__snippet" href="//duckduckgo.com/l/?uddg=x">
            A concise   snippet of text.
          </a>
        </div>"#;
        let results = parse_html_results(html);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com/page");
        assert_eq!(results[0].title, "Example Title");
        assert_eq!(results[0].snippet, "A concise snippet of text.");
    }

    #[test]
    fn parse_html_skips_rows_without_a_decodable_url() {
        // A row whose anchor lacks an href yields no result rather than panics.
        let html = r#"<div class="result web-result">
            <a class="result__a">No href</a></div>"#;
        assert!(parse_html_results(html).is_empty());
        // Malformed / empty HTML never panics.
        assert!(parse_html_results("<not really html").is_empty());
        assert!(parse_html_results("").is_empty());
    }

    #[test]
    fn parse_lite_zips_links_to_snippets() {
        let html = r#"
        <table>
          <tr><td>1.</td><td>
            <a class="result-link" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.example%2F">Alpha</a>
          </td></tr>
          <tr><td class="result-snippet">First snippet.</td></tr>
          <tr><td>2.</td><td>
            <a class="result-link" href="https://b.example/direct">Beta</a>
          </td></tr>
          <tr><td class="result-snippet">Second snippet.</td></tr>
        </table>"#;
        let results = parse_lite_results(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://a.example/");
        assert_eq!(results[0].title, "Alpha");
        assert_eq!(results[0].snippet, "First snippet.");
        // A direct (non-tracker) absolute href passes through untouched.
        assert_eq!(results[1].url, "https://b.example/direct");
        assert_eq!(results[1].snippet, "Second snippet.");
    }

    #[test]
    fn throttle_detection_covers_202_and_body_markers() {
        assert!(is_throttled(202, ""));
        assert!(is_throttled(200, "If this error persists... Ratelimit"));
        assert!(is_throttled(
            200,
            "Please verify you are a human to continue"
        ));
        assert!(!is_throttled(
            200,
            "<html><body>normal results</body></html>"
        ));
    }

    #[test]
    fn decode_uddg_rejects_non_http_targets() {
        // uddg pointing at a javascript: URL is refused.
        let href = "//duckduckgo.com/l/?uddg=javascript%3Aalert(1)";
        assert!(decode_uddg(href).is_none());
        assert!(decode_uddg("").is_none());
    }

    #[test]
    fn finalize_reports_recency_and_country_unsupported() {
        let rows = vec![SearchResult {
            title: "t".into(),
            url: "https://example.com/".into(),
            snippet: "s".into(),
        }];
        let out = finalize(rows, &query(Some(super::super::Recency::Week), Some("us")));
        assert_eq!(out.results.len(), 1);
        let unsupported: Vec<_> = out
            .filters
            .iter()
            .filter(|f| f.enforcement == FilterEnforcement::Unsupported)
            .map(|f| f.filter.as_str())
            .collect();
        assert!(unsupported.contains(&"recency"));
        assert!(unsupported.contains(&"country"));
    }

    #[test]
    fn finalize_truncates_to_max_results() {
        let rows: Vec<SearchResult> = (0..5)
            .map(|i| SearchResult {
                title: format!("t{i}"),
                url: format!("https://example.com/{i}"),
                snippet: String::new(),
            })
            .collect();
        let mut q = query(None, None);
        q.max_results = 3;
        let out = finalize(rows, &q);
        assert_eq!(out.results.len(), 3);
        assert!(out.filters.is_empty());
    }

    #[test]
    fn finalize_filters_before_truncating() {
        // A matching include-domain row ranked AFTER the cap must survive: filter
        // first, then truncate (regression for the truncate-before-filter bug).
        let rows = vec![
            SearchResult {
                title: "a".into(),
                url: "https://other.com/1".into(),
                snippet: String::new(),
            },
            SearchResult {
                title: "b".into(),
                url: "https://other.com/2".into(),
                snippet: String::new(),
            },
            SearchResult {
                title: "c".into(),
                url: "https://keep.com/3".into(),
                snippet: String::new(),
            },
        ];
        let mut q = query(None, None);
        q.max_results = 1;
        q.include_domains = vec!["keep.com".into()];
        let out = finalize(rows, &q);
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].url, "https://keep.com/3");
    }
}
