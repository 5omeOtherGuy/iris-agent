//! Brave Search API backend for `web_search`.
//!
//! Calls `https://api.search.brave.com/res/v1/web/search` with the
//! `X-Subscription-Token` header. Search only -- Brave has no reader surface, so
//! this module never touches the pinned fetch path. A key is mandatory: the free
//! `Data for AI` subscription tier is enough, but with none we fail with an
//! actionable configuration error rather than a silent empty result.
//!
//! Brave's free tier is rate-limited to ~1 request/second, so we serialize
//! issuance behind a process-wide timestamp and space calls at least one second
//! apart. The tool is exclusive (one web call in flight at a time), so a plain
//! `Mutex<Option<Instant>>` is sufficient; we never hold the guard across the
//! sleep await.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::super::fetch::{build_api_client, send_api};
use super::super::{FilterEnforcement, MAX_API_BYTES, SearchResult};
use super::filters::{apply_domain_filter, brave_freshness, report};
use super::{SearchOutcome, SearchQuery};

/// Brave Search API endpoint.
const BRAVE_SEARCH_BASE: &str = "https://api.search.brave.com/res/v1/web/search";

/// Brave caps `count` at 20; clamp defensively in case a caller passes more.
const MAX_COUNT: usize = 20;

/// Minimum spacing between Brave requests to respect the free tier's ~1 req/s
/// throttle.
const MIN_SPACING: Duration = Duration::from_secs(1);

/// Process-wide last-issue timestamp for client-side spacing. Holds the instant
/// the most recent request was (or will be) issued, so the next caller can
/// compute how long to wait.
static LAST_CALL: Mutex<Option<Instant>> = Mutex::new(None);

/// Brave web-search backend. Requires `key`; issues one throttled request and
/// normalizes `web.results[]` into [`SearchResult`]s, then applies domain
/// post-filters and records native country/recency enforcement.
pub(super) async fn search(
    key: Option<&str>,
    query: &SearchQuery,
    cancel: &CancellationToken,
) -> anyhow::Result<SearchOutcome> {
    let key = key
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "web_search via Brave requires an API key. Set BRAVE_API_KEY (or the Brave key row \
             in web-tools settings); a free `Data for AI` subscription key is sufficient."
            )
        })?;

    // Respect the ~1 req/s free-tier limit before issuing. Reserve the slot and
    // drop the guard, then sleep -- never awaiting while holding the lock.
    throttle(cancel).await;

    // When domain post-filters are set, request up to the API max so filtered-
    // out rows leave headroom for replacements; otherwise request exactly what
    // the caller wants. The final list is always truncated to `max_results`.
    let has_domain_filters = !query.include_domains.is_empty() || !query.exclude_domains.is_empty();
    let requested = if has_domain_filters {
        MAX_COUNT
    } else {
        query.max_results.clamp(1, MAX_COUNT)
    };
    let count = requested.to_string();
    let client = build_api_client().map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut request = client
        .get(BRAVE_SEARCH_BASE)
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json")
        .query(&[("q", query.query.as_str()), ("count", count.as_str())]);

    // Country and recency map natively to Brave's `country` / `freshness`.
    if let Some(country) = query.country.as_deref() {
        request = request.query(&[("country", country.to_ascii_uppercase().as_str())]);
    }
    if let Some(recency) = query.recency {
        request = request.query(&[("freshness", brave_freshness(recency))]);
    }

    let (status, body, _truncated) = send_api(request, MAX_API_BYTES, cancel)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if status != 200 {
        let hint = match status {
            401 | 403 => " (check the API key)",
            429 => " (rate-limited; slow down or upgrade the plan)",
            _ => "",
        };
        anyhow::bail!("Brave search failed: HTTP {status}{hint}");
    }

    let results = parse_brave_json(&body)?;
    let (mut results, domain_reports) =
        apply_domain_filter(results, &query.include_domains, &query.exclude_domains);
    // Defensively enforce the caller's ceiling regardless of what Brave returned.
    results.truncate(query.max_results);

    let mut filters = domain_reports;
    if query.country.is_some() {
        filters.push(report("country", FilterEnforcement::Native));
    }
    if query.recency.is_some() {
        filters.push(report("recency", FilterEnforcement::Native));
    }

    Ok(SearchOutcome { results, filters })
}

/// Block until at least [`MIN_SPACING`] has elapsed since the previous issue,
/// then record this call's issue time. The timestamp update happens under the
/// lock; the sleep happens after the guard is dropped. Cancellation short-cuts
/// the wait (the caller's request will then be cancelled in `send_api`).
async fn throttle(cancel: &CancellationToken) {
    let wait = {
        let mut last = LAST_CALL.lock().expect("brave throttle mutex poisoned");
        let now = Instant::now();
        let wait = match *last {
            Some(prev) => MIN_SPACING.saturating_sub(now.saturating_duration_since(prev)),
            None => Duration::ZERO,
        };
        // Reserve this call's slot at its projected issue time so a subsequent
        // caller spaces off of us, not off the previous request.
        *last = Some(now + wait);
        wait
    };
    if !wait.is_zero() {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {}
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// Brave's JSON result envelope: hits live under `web.results[]`, each with
/// `title`, `url`, and `description`. Everything is optional -- Brave omits
/// blocks it has no data for, and a malformed entry must not sink the batch.
#[derive(Debug, Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Debug, Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Debug, Deserialize)]
struct BraveResult {
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
}

/// Parse a Brave web-search JSON body into normalized results. Pure and
/// fixture-testable: no network, no throttle. Entries missing a `url` are
/// dropped (a hit with no link is useless); `description` maps to `snippet`.
fn parse_brave_json(body: &[u8]) -> anyhow::Result<Vec<SearchResult>> {
    let parsed: BraveResponse = serde_json::from_slice(body)
        .map_err(|e| anyhow::anyhow!("Brave search returned unparsable JSON: {e}"))?;
    let Some(web) = parsed.web else {
        return Ok(Vec::new());
    };
    let results = web
        .results
        .into_iter()
        .filter_map(|r| {
            let url = r.url?;
            Some(SearchResult {
                title: r.title.unwrap_or_default(),
                url,
                snippet: r.description.unwrap_or_default(),
            })
        })
        .collect();
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query() -> SearchQuery {
        SearchQuery {
            query: "rust async".into(),
            max_results: 5,
            include_domains: Vec::new(),
            exclude_domains: Vec::new(),
            recency: None,
            country: None,
        }
    }

    #[test]
    fn parses_web_results_and_maps_description_to_snippet() {
        let body = br#"{
            "web": {
                "results": [
                    {"title": "Tokio", "url": "https://tokio.rs", "description": "async runtime"},
                    {"title": "Async book", "url": "https://rust-lang.github.io/async-book", "description": "the book"}
                ]
            }
        }"#;
        let results = parse_brave_json(body).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Tokio");
        assert_eq!(results[0].url, "https://tokio.rs");
        assert_eq!(results[0].snippet, "async runtime");
    }

    #[test]
    fn missing_web_block_yields_empty() {
        assert!(parse_brave_json(b"{}").unwrap().is_empty());
        assert!(parse_brave_json(br#"{"web": {}}"#).unwrap().is_empty());
    }

    #[test]
    fn entries_without_url_are_dropped_and_missing_fields_default() {
        let body = br#"{"web": {"results": [
            {"title": "no url"},
            {"url": "https://example.com"}
        ]}}"#;
        let results = parse_brave_json(body).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com");
        assert_eq!(results[0].title, "");
        assert_eq!(results[0].snippet, "");
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_brave_json(b"not json").is_err());
    }

    #[tokio::test]
    async fn no_key_is_an_actionable_error() {
        let err = search(None, &query(), &CancellationToken::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("BRAVE_API_KEY"), "message was: {err}");

        // An all-whitespace key is treated as absent.
        let err = search(Some("  "), &query(), &CancellationToken::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("BRAVE_API_KEY"), "message was: {err}");
    }

    #[test]
    fn country_and_recency_report_native() {
        let results = vec![SearchResult {
            title: "t".into(),
            url: "https://x.com".into(),
            snippet: "s".into(),
        }];
        let (results, domain_reports) = apply_domain_filter(results, &[], &[]);
        let mut filters = domain_reports;
        // Mirror the search() assembly with both native filters requested.
        filters.push(report("country", FilterEnforcement::Native));
        filters.push(report("recency", FilterEnforcement::Native));

        assert!(!results.is_empty());
        let country = filters.iter().find(|f| f.filter == "country").unwrap();
        assert_eq!(country.enforcement, FilterEnforcement::Native);
        let recency = filters.iter().find(|f| f.filter == "recency").unwrap();
        assert_eq!(recency.enforcement, FilterEnforcement::Native);
    }
}
