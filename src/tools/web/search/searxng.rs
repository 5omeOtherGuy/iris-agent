//! SearXNG JSON backend for `web_search`.
//!
//! Queries a self-hosted SearXNG instance's JSON API
//! (`GET {searxngUrl}/search?format=json`). Unlike Brave/Jina this endpoint is
//! not a hardcoded SaaS URL -- it is the operator's own instance, supplied by
//! the GLOBAL-ONLY, trusted `searxngUrl` setting. Because it is trusted config
//! (and may legitimately be a private/LAN address), the base URL itself is NOT
//! run through the SSRF policy; it goes through the normal API client, same as
//! Brave/Jina. The RESULT URLs it returns are third-party and untrusted, so they
//! are still sanitized by the output URL policy in [`filters`](super::filters).
//!
//! Filter honesty:
//! - `safesearch` is always sent (moderate) so results are filtered upstream.
//! - `recency` maps natively to SearXNG's `time_range` (reported `Native`).
//! - `include_domains`/`exclude_domains` are post-filtered locally: SearXNG
//!   aggregates many engines and does not honor `site:` uniformly, so claiming
//!   native domain filtering would be dishonest.
//! - `country` has no reliable SearXNG parameter (it exposes `language`, not a
//!   region), so it is reported `Unsupported` rather than faked.
//!
//! Post-filter headroom: SearXNG returns a full page of results, and we filter
//! BEFORE truncating to `max_results`, so domain-filtered rows never starve the
//! final list.

use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::super::fetch::{build_api_client, send_api_with};
use super::super::{FilterEnforcement, SearchResult, WebToolsConfig};
use super::filters::{apply_domain_filter, report};
use super::{Recency, SearchOutcome, SearchQuery};

/// Moderate safe-search (`0`=off, `1`=moderate, `2`=strict). Always sent so an
/// instance defaulting to off still filters adult/spam results.
const SAFE_SEARCH: &str = "1";

/// Run a SearXNG search: build the JSON request against the configured instance,
/// fetch a size-capped body, parse `results[]`, then post-filter domains and
/// record the truthful per-filter reports.
pub(super) async fn search(
    config: &WebToolsConfig,
    query: &SearchQuery,
    cancel: &CancellationToken,
) -> anyhow::Result<SearchOutcome> {
    let base = config
        .searxng_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "web_search via SearXNG requires a trusted instance URL. Set `searxngUrl` in \
                 global settings to your SearXNG base URL (e.g. https://searx.example.org)."
            )
        })?;

    let endpoint = build_endpoint(base, query)?;

    let client = build_api_client()
        .map_err(|e| anyhow::anyhow!("failed to build SearXNG HTTP client: {e}"))?;
    let request = client.get(endpoint).header("Accept", "application/json");

    let (status, body, _truncated) = send_api_with(
        request,
        config.max_search_response_bytes,
        config.search_timeout,
        cancel,
    )
    .await
    .map_err(|e| anyhow::anyhow!("SearXNG search request failed: {e}"))?;

    if status != 200 {
        let hint = match status {
            403 => {
                " (the instance may not permit the JSON format; enable `format: [json]` in its settings)"
            }
            429 => " (rate-limited by the instance; slow down or use your own)",
            _ => "",
        };
        anyhow::bail!("SearXNG search returned HTTP {status}{hint}");
    }

    let results = parse_searxng_json(&body)?;

    // Domain filters are post-filtered locally (SearXNG has no uniform `site:`).
    // Filtering also sanitizes result URLs and runs BEFORE truncation.
    let (mut results, mut filters) =
        apply_domain_filter(results, &query.include_domains, &query.exclude_domains);
    results.truncate(query.max_results);

    if query.recency.is_some() {
        filters.push(report("recency", FilterEnforcement::Native));
    }
    if query.country.is_some() {
        filters.push(report("country", FilterEnforcement::Unsupported));
    }

    Ok(SearchOutcome { results, filters })
}

/// Build the `{base}/search` JSON endpoint URL with all query parameters. The
/// base is joined with a guaranteed trailing slash so an instance mounted under
/// a subpath (`https://host/searxng`) keeps that prefix. `url` percent-encodes
/// every value, so the raw query can never inject extra parameters.
fn build_endpoint(base: &str, query: &SearchQuery) -> anyhow::Result<Url> {
    let mut normalized = base.to_string();
    if !normalized.ends_with('/') {
        normalized.push('/');
    }
    let root = Url::parse(&normalized)
        .map_err(|e| anyhow::anyhow!("searxngUrl is not a valid URL: {e}"))?;
    let endpoint = root
        .join("search")
        .map_err(|e| anyhow::anyhow!("failed to build the SearXNG search URL: {e}"))?;

    let mut params: Vec<(&str, &str)> = vec![
        ("q", query.query.as_str()),
        ("format", "json"),
        ("safesearch", SAFE_SEARCH),
    ];
    if let Some(recency) = query.recency {
        params.push(("time_range", time_range(recency)));
    }

    Url::parse_with_params(endpoint.as_str(), &params)
        .map_err(|e| anyhow::anyhow!("failed to build the SearXNG query URL: {e}"))
}

/// Map a recency window to SearXNG's `time_range` value.
fn time_range(recency: Recency) -> &'static str {
    match recency {
        Recency::Day => "day",
        Recency::Week => "week",
        Recency::Month => "month",
        Recency::Year => "year",
    }
}

/// SearXNG JSON envelope: hits live under `results[]`, each with `url`, `title`,
/// and `content`. All fields are optional so a malformed row cannot sink the
/// batch; a row without a `url` is useless and skipped.
#[derive(Debug, Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Debug, Deserialize)]
struct SearxngResult {
    url: Option<String>,
    title: Option<String>,
    content: Option<String>,
}

/// Parse a SearXNG JSON body into normalized results. Pure and fixture-testable:
/// no network, no filtering. Entries without a non-empty `url` are dropped;
/// `content` maps to `snippet`; missing `title`/`content` default to empty.
fn parse_searxng_json(body: &[u8]) -> anyhow::Result<Vec<SearchResult>> {
    let parsed: SearxngResponse = serde_json::from_slice(body)
        .map_err(|e| anyhow::anyhow!("SearXNG search returned unparsable JSON: {e}"))?;
    let results = parsed
        .results
        .into_iter()
        .filter_map(|r| {
            let url = r.url?;
            if url.trim().is_empty() {
                return None;
            }
            Some(SearchResult {
                title: r.title.unwrap_or_default(),
                url,
                snippet: r.content.unwrap_or_default(),
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
    fn parses_results_and_maps_content_to_snippet() {
        let body = br#"{
            "query": "rust async",
            "results": [
                {"url": "https://tokio.rs", "title": "Tokio", "content": "async runtime"},
                {"url": "https://rust-lang.github.io/async-book", "title": "Async book", "content": "the book"}
            ]
        }"#;
        let results = parse_searxng_json(body).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Tokio");
        assert_eq!(results[0].url, "https://tokio.rs");
        assert_eq!(results[0].snippet, "async runtime");
    }

    #[test]
    fn missing_results_and_urlless_rows_are_dropped() {
        assert!(parse_searxng_json(b"{}").unwrap().is_empty());
        let body = br#"{"results": [
            {"title": "no url"},
            {"url": "  "},
            {"url": "https://example.com"}
        ]}"#;
        let results = parse_searxng_json(body).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com");
        assert_eq!(results[0].title, "");
        assert_eq!(results[0].snippet, "");
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_searxng_json(b"not json").is_err());
    }

    #[test]
    fn build_endpoint_appends_search_and_encodes_params() {
        let mut q = query();
        q.query = "a b&c".into();
        q.recency = Some(Recency::Week);
        let url = build_endpoint("https://searx.example.org", &q).unwrap();
        assert_eq!(url.path(), "/search");
        assert_eq!(url.host_str(), Some("searx.example.org"));
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert!(pairs.contains(&("q".into(), "a b&c".into())));
        assert!(pairs.contains(&("format".into(), "json".into())));
        assert!(pairs.contains(&("safesearch".into(), "1".into())));
        assert!(pairs.contains(&("time_range".into(), "week".into())));
    }

    #[test]
    fn build_endpoint_preserves_a_subpath_mount() {
        let url = build_endpoint("https://host.example/searxng", &query()).unwrap();
        assert_eq!(url.path(), "/searxng/search");
    }

    #[test]
    fn build_endpoint_omits_time_range_without_recency() {
        let url = build_endpoint("https://searx.example.org/", &query()).unwrap();
        assert!(!url.query().unwrap_or_default().contains("time_range"));
    }

    #[tokio::test]
    async fn missing_url_is_an_actionable_error() {
        let err = search(
            &WebToolsConfig::default(),
            &query(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("searxngUrl"), "message was: {err}");
        // An all-whitespace URL is treated as absent.
        let err = search(
            &WebToolsConfig {
                searxng_url: Some("   ".into()),
                ..WebToolsConfig::default()
            },
            &query(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("searxngUrl"), "message was: {err}");
    }

    #[test]
    fn recency_reports_native_and_country_unsupported() {
        // Mirror the tail of `search`: recency -> Native, country -> Unsupported.
        let (_out, mut filters) = apply_domain_filter(Vec::new(), &[], &[]);
        let recency: Option<Recency> = Some(Recency::Month);
        let country: Option<String> = Some("de".into());
        if recency.is_some() {
            filters.push(report("recency", FilterEnforcement::Native));
        }
        if country.is_some() {
            filters.push(report("country", FilterEnforcement::Unsupported));
        }
        let r = filters.iter().find(|f| f.filter == "recency").unwrap();
        assert_eq!(r.enforcement, FilterEnforcement::Native);
        let c = filters.iter().find(|f| f.filter == "country").unwrap();
        assert_eq!(c.enforcement, FilterEnforcement::Unsupported);
    }
}
