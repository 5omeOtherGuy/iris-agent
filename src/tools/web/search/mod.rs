//! `web_search` backends: shared request/response types plus the backend
//! dispatch. Each backend is a `pub(super) async fn search(...)` returning a
//! normalized [`SearchOutcome`]; the dispatch [`run_search`] selects one by the
//! resolved [`SearchBackend`]. Adding a backend (e.g. SearXNG) is a new module
//! plus one match arm -- the "trait as seam" the plan calls for, expressed as a
//! match to avoid an `async_trait` dependency.

mod brave;
mod duckduckgo;
mod filters;
mod jina;

// Backends reach the shared filter helpers via `super::filters::{...}`.

// Re-exported for the token-efficiency corpus (`web::corpus`) so it can measure
// the real raw-HTML -> `SearchResult` seam without duplicating the parser
// (ADR-0036 rule 5). Test-only: the production dispatch uses the parser in
// place inside `duckduckgo::scrape`.
#[cfg(test)]
pub(super) use duckduckgo::parse_html_results;

use tokio_util::sync::CancellationToken;

use super::{FilterReport, SearchBackend, SearchResult, WebToolsConfig};

/// A parsed, validated search request (from the tool arguments).
#[derive(Debug, Clone)]
pub(super) struct SearchQuery {
    pub(super) query: String,
    pub(super) max_results: usize,
    pub(super) include_domains: Vec<String>,
    pub(super) exclude_domains: Vec<String>,
    pub(super) recency: Option<Recency>,
    pub(super) country: Option<String>,
}

/// Recency window for freshness filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Recency {
    Day,
    Week,
    Month,
    Year,
}

impl Recency {
    pub(super) fn parse(value: &str) -> Option<Self> {
        match value {
            "day" => Some(Self::Day),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            "year" => Some(Self::Year),
            _ => None,
        }
    }
}

/// Normalized result of a search: ranked hits plus the truthful per-filter
/// enforcement report.
#[derive(Debug, Clone)]
pub(super) struct SearchOutcome {
    pub(super) results: Vec<SearchResult>,
    pub(super) filters: Vec<FilterReport>,
}

/// Dispatch a search to the resolved backend. Backends surface actionable
/// errors (missing key, throttle, markup drift) as `anyhow::Error`; there is no
/// silent cross-backend fallback (plan §4: fail with a named cause instead).
pub(super) async fn run_search(
    backend: SearchBackend,
    config: &WebToolsConfig,
    query: &SearchQuery,
    cancel: &CancellationToken,
) -> anyhow::Result<SearchOutcome> {
    match backend {
        SearchBackend::Native => duckduckgo::search(query, cancel).await,
        SearchBackend::Brave => brave::search(config.brave_key.as_deref(), query, cancel).await,
        SearchBackend::Jina => jina::search(config.jina_key.as_deref(), query, cancel).await,
    }
}
