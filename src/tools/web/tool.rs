//! Tool-facing entry points: argument parsing, dispatch into the backends,
//! untrusted-content framing, and [`ToolOutput`] assembly. The thin `Tool`
//! impls in `src/tools/web_search.rs` and `src/tools/read_web_page.rs` delegate
//! here so all model-facing shaping lives in one place.

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::nexus::ToolOutput;

use super::fetch::SystemResolver;
use super::read::{self, ReadRequest};
use super::search::{self, Recency, SearchQuery};
use super::{
    FilterReport, PageResult, ReadBackend, SearchBackend, SearchResult, WebToolsConfig,
    frame_untrusted,
};

/// Default number of search results when the caller omits `max_results`.
const DEFAULT_MAX_RESULTS: usize = 5;
/// Hard ceiling on requested results (keeps the token cost bounded).
const MAX_RESULTS_CEILING: usize = 10;

/// Execute a `web_search` call: parse + validate args, dispatch to the resolved
/// backend, and return framed, ranked results with a truthful filter report in
/// `metadata`.
pub(crate) async fn execute_web_search(
    config: &WebToolsConfig,
    backend: SearchBackend,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<ToolOutput> {
    let query = parse_search_query(args)?;
    let outcome = search::run_search(backend, config, &query, cancel).await?;

    let body = render_results(&outcome.results);
    let text = frame_untrusted(&format!("search: {}", query.query), backend.as_str(), &body);

    let mut metadata = Map::new();
    metadata.insert("backend".into(), Value::String(backend.as_str().into()));
    metadata.insert(
        "result_count".into(),
        Value::Number(outcome.results.len().into()),
    );
    metadata.insert("filters".into(), filters_to_value(&outcome.filters));

    Ok(ToolOutput {
        content: text,
        metadata,
    })
}

/// Execute a `read_web_page` call: parse args, dispatch to the resolved reader
/// (native pinned fetch or Jina), and return framed content plus read metadata.
pub(crate) async fn execute_read_web_page(
    config: &WebToolsConfig,
    backend: ReadBackend,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<ToolOutput> {
    let request = parse_read_request(args)?;
    let resolver = SystemResolver;
    let page = read::run_read(backend, config, &request, &resolver, cancel).await?;

    let text = frame_untrusted(&page.final_url, backend.as_str(), &page.content);
    Ok(ToolOutput {
        content: text,
        metadata: read_metadata(backend.as_str(), &page),
    })
}

/// Parse and validate `web_search` arguments.
fn parse_search_query(args: &Value) -> Result<SearchQuery> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or_else(|| anyhow!("web_search requires a non-empty `query` string"))?
        .to_string();

    let max_results = match args.get("max_results") {
        None | Some(Value::Null) => DEFAULT_MAX_RESULTS,
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| anyhow!("`max_results` must be a positive integer"))?;
            (n as usize).clamp(1, MAX_RESULTS_CEILING)
        }
    };

    let include_domains = parse_string_array(args, "include_domains")?;
    let exclude_domains = parse_string_array(args, "exclude_domains")?;

    let recency = match args.get("recency").and_then(Value::as_str) {
        None => None,
        Some(v) => Some(
            Recency::parse(v)
                .ok_or_else(|| anyhow!("`recency` must be one of day|week|month|year"))?,
        ),
    };

    let country = match args.get("country").and_then(Value::as_str) {
        None => None,
        Some(v) => {
            let v = v.trim();
            if v.len() != 2 || !v.chars().all(|c| c.is_ascii_alphabetic()) {
                bail!("`country` must be an ISO 3166-1 alpha-2 code (e.g. \"us\")");
            }
            Some(v.to_ascii_lowercase())
        }
    };

    Ok(SearchQuery {
        query,
        max_results,
        include_domains,
        exclude_domains,
        recency,
        country,
    })
}

/// Parse and validate `read_web_page` arguments.
fn parse_read_request(args: &Value) -> Result<ReadRequest> {
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| anyhow!("read_web_page requires a non-empty `url` string"))?
        .to_string();

    let objective = args
        .get("objective")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .map(str::to_string);

    Ok(ReadRequest { url, objective })
}

/// Parse an optional array-of-strings argument, rejecting a non-array or a
/// non-string element rather than silently ignoring it.
fn parse_string_array(args: &Value, key: &str) -> Result<Vec<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("`{key}` must be an array of strings"))
            })
            .collect(),
        Some(_) => bail!("`{key}` must be an array of strings"),
    }
}

/// Render ranked results as a compact `title / url / snippet` list.
fn render_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "No results.".to_string();
    }
    let mut out = String::new();
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
        if !r.snippet.trim().is_empty() {
            out.push_str(&format!("   {}\n", r.snippet));
        }
    }
    out
}

/// Serialize the filter reports for `metadata.filters`.
fn filters_to_value(filters: &[FilterReport]) -> Value {
    Value::Array(filters.iter().map(FilterReport::to_value).collect())
}

/// Build the `read_web_page` metadata object.
fn read_metadata(backend: &str, page: &PageResult) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("backend".into(), Value::String(backend.into()));
    m.insert("final_url".into(), Value::String(page.final_url.clone()));
    m.insert("status".into(), Value::Number(page.status.into()));
    m.insert("truncated".into(), Value::Bool(page.truncated));
    m.insert("redirects".into(), Value::Number(page.redirects.into()));
    if let Some(title) = &page.title {
        m.insert("title".into(), Value::String(title.clone()));
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_search_defaults_and_clamps() {
        let q = parse_search_query(&json!({"query": "rust ssrf"})).unwrap();
        assert_eq!(q.query, "rust ssrf");
        assert_eq!(q.max_results, DEFAULT_MAX_RESULTS);

        let q = parse_search_query(&json!({"query": "x", "max_results": 99})).unwrap();
        assert_eq!(q.max_results, MAX_RESULTS_CEILING);
        let q = parse_search_query(&json!({"query": "x", "max_results": 0})).unwrap();
        assert_eq!(q.max_results, 1);
    }

    #[test]
    fn parse_search_rejects_bad_input() {
        assert!(parse_search_query(&json!({})).is_err());
        assert!(parse_search_query(&json!({"query": "  "})).is_err());
        assert!(parse_search_query(&json!({"query": "x", "recency": "decade"})).is_err());
        assert!(parse_search_query(&json!({"query": "x", "country": "usa"})).is_err());
        assert!(parse_search_query(&json!({"query": "x", "include_domains": "docs.rs"})).is_err());
    }

    #[test]
    fn parse_search_accepts_filters() {
        let q = parse_search_query(&json!({
            "query": "x",
            "include_domains": ["docs.rs", "example.com"],
            "recency": "week",
            "country": "US",
        }))
        .unwrap();
        assert_eq!(q.include_domains.len(), 2);
        assert_eq!(q.recency, Some(Recency::Week));
        assert_eq!(q.country.as_deref(), Some("us"));
    }

    #[test]
    fn parse_read_request_variants() {
        let r = parse_read_request(&json!({"url": "https://example.com"})).unwrap();
        assert_eq!(r.url, "https://example.com");
        assert!(r.objective.is_none());
        let r =
            parse_read_request(&json!({"url": "https://x.com", "objective": "pricing"})).unwrap();
        assert_eq!(r.objective.as_deref(), Some("pricing"));
        assert!(parse_read_request(&json!({})).is_err());
    }

    #[test]
    fn render_results_formats_and_handles_empty() {
        assert_eq!(render_results(&[]), "No results.");
        let body = render_results(&[SearchResult {
            title: "T".into(),
            url: "https://u".into(),
            snippet: "S".into(),
        }]);
        assert!(body.contains("1. T"));
        assert!(body.contains("https://u"));
        assert!(body.contains("S"));
    }

    #[test]
    fn framing_marks_content_untrusted() {
        let framed = frame_untrusted("https://x.com", "native", "body text");
        assert!(framed.contains("untrusted"));
        assert!(framed.contains("https://x.com"));
        assert!(framed.contains("body text"));
    }
}
