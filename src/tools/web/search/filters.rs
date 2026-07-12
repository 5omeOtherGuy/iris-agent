//! Shared domain/recency/country filter logic for the search backends, ported
//! from ampi-web's `filters.ts`.
//!
//! Domains are always honorable: a backend that cannot express them upstream
//! post-filters parsed results on hostname (suffix-aware, via
//! [`policy::host_matches_domain`](super::super::policy::host_matches_domain)).
//! Recency/country are "native or nothing" -- a backend with a real freshness
//! parameter maps it; one without reliable dates reports the filter as
//! `unsupported` rather than faking it. Every requested filter yields exactly
//! one [`FilterReport`]; nothing is silently dropped.

use url::Url;

use super::super::policy::host_matches_domain;
use super::super::{FilterEnforcement, FilterReport, SearchResult};
use super::Recency;

/// Brave `freshness` code for a recency window.
pub(super) fn brave_freshness(recency: Recency) -> &'static str {
    match recency {
        Recency::Day => "pd",
        Recency::Week => "pw",
        Recency::Month => "pm",
        Recency::Year => "py",
    }
}

/// Parse a result's hostname, or `None` when the URL is unparsable.
fn hostname_of(url: &str) -> Option<String> {
    Url::parse(url).ok()?.host_str().map(|h| h.to_string())
}

/// Apply include/exclude domain filters over parsed results and emit one
/// `post_filter` report per non-empty filter.
///
/// - include: keep only results whose host matches one domain; a result with
///   no parseable URL cannot match and is dropped.
/// - exclude: drop results whose host matches one domain; a result with no
///   parseable URL is kept (it cannot be matched to exclude it).
pub(super) fn apply_domain_filter(
    results: Vec<SearchResult>,
    include: &[String],
    exclude: &[String],
) -> (Vec<SearchResult>, Vec<FilterReport>) {
    let include: Vec<&String> = include.iter().filter(|d| !d.trim().is_empty()).collect();
    let exclude: Vec<&String> = exclude.iter().filter(|d| !d.trim().is_empty()).collect();
    let mut reports = Vec::new();
    let mut out = results;

    if !include.is_empty() {
        out.retain(|row| match hostname_of(&row.url) {
            Some(host) => include.iter().any(|d| host_matches_domain(&host, d)),
            None => false,
        });
        reports.push(report("include_domains", FilterEnforcement::PostFilter));
    }
    if !exclude.is_empty() {
        out.retain(|row| match hostname_of(&row.url) {
            Some(host) => !exclude.iter().any(|d| host_matches_domain(&host, d)),
            None => true,
        });
        reports.push(report("exclude_domains", FilterEnforcement::PostFilter));
    }

    (out, reports)
}

/// Build a single filter report.
pub(super) fn report(filter: &str, enforcement: FilterEnforcement) -> FilterReport {
    FilterReport {
        filter: filter.to_string(),
        enforcement,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(url: &str) -> SearchResult {
        SearchResult {
            title: "t".into(),
            url: url.into(),
            snippet: "s".into(),
        }
    }

    #[test]
    fn include_keeps_only_matching_hosts() {
        let results = vec![
            hit("https://docs.rs/a"),
            hit("https://example.com/b"),
            hit("https://api.docs.rs/c"),
        ];
        let (out, reports) = apply_domain_filter(results, &["docs.rs".to_string()], &[]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|r| r.url.contains("docs.rs")));
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].enforcement, FilterEnforcement::PostFilter);
    }

    #[test]
    fn exclude_drops_matching_hosts_and_keeps_unparsable() {
        let results = vec![
            hit("https://spam.com/a"),
            hit("https://good.org/b"),
            hit("not a url"),
        ];
        let (out, reports) = apply_domain_filter(results, &[], &["spam.com".to_string()]);
        // spam.com dropped; good.org kept; unparsable kept (cannot be excluded).
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|r| r.url == "not a url"));
        assert_eq!(reports.len(), 1);
    }

    #[test]
    fn empty_filters_report_nothing() {
        let (out, reports) = apply_domain_filter(vec![hit("https://x.com/")], &[], &[]);
        assert_eq!(out.len(), 1);
        assert!(reports.is_empty());
    }

    #[test]
    fn brave_freshness_codes() {
        assert_eq!(brave_freshness(Recency::Day), "pd");
        assert_eq!(brave_freshness(Recency::Year), "py");
    }
}
