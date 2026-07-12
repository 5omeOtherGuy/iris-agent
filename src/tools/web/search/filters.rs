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

use super::super::policy::{host_matches_domain, validate_external_url};
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

/// Drop every result whose URL fails the public SSRF/URL policy, TEXT-ONLY (no
/// DNS). Backends return links straight from a third party, so this is the
/// output half of the policy that guards fetched pages: a result pointing at
/// `http://169.254.169.254/`, a private IP literal, a non-http scheme, a
/// credentialed URL, or garbage never reaches the model. Returns the surviving
/// results and the number dropped so the caller can report it truthfully.
pub(super) fn sanitize_result_urls(results: Vec<SearchResult>) -> (Vec<SearchResult>, usize) {
    let before = results.len();
    let kept: Vec<SearchResult> = results
        .into_iter()
        .filter(|row| validate_external_url(&row.url).is_ok())
        .collect();
    let dropped = before - kept.len();
    (kept, dropped)
}

/// Apply include/exclude domain filters over parsed results and emit one
/// `post_filter` report per non-empty filter. Runs [`sanitize_result_urls`]
/// FIRST so disallowed URLs are dropped before filtering or truncation, then
/// records a `result_url_policy` report (with a reason) when any were removed.
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

    // Output URL policy: drop disallowed/invalid result URLs before anything else.
    let (mut out, dropped) = sanitize_result_urls(results);
    if dropped > 0 {
        reports.push(report_with_reason(
            "result_url_policy",
            FilterEnforcement::PostFilter,
            format!("dropped {dropped} result(s) whose URL failed the public URL policy"),
        ));
    }

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

/// Build a single filter report with no attached reason.
pub(super) fn report(filter: &str, enforcement: FilterEnforcement) -> FilterReport {
    FilterReport {
        filter: filter.to_string(),
        enforcement,
        reason: None,
    }
}

/// Build a filter report carrying a human-readable reason (surfaced in
/// `metadata.filters`), e.g. why a domain was ignored or results were dropped.
pub(super) fn report_with_reason(
    filter: &str,
    enforcement: FilterEnforcement,
    reason: impl Into<String>,
) -> FilterReport {
    FilterReport {
        filter: filter.to_string(),
        enforcement,
        reason: Some(reason.into()),
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
    fn exclude_drops_matching_hosts_and_url_policy_drops_invalid() {
        let results = vec![
            hit("https://spam.com/a"),
            hit("https://good.org/b"),
            hit("not a url"),
        ];
        let (out, reports) = apply_domain_filter(results, &[], &["spam.com".to_string()]);
        // "not a url" fails the URL policy and is dropped first; spam.com is then
        // excluded; good.org survives.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://good.org/b");
        // One url_policy report (invalid URL) + one exclude_domains report.
        assert!(reports.iter().any(|r| r.filter == "result_url_policy"));
        assert!(reports.iter().any(|r| r.filter == "exclude_domains"));
    }

    #[test]
    fn sanitize_drops_private_and_non_http_result_urls() {
        let results = vec![
            hit("https://ok.example/a"),
            hit("http://169.254.169.254/latest/meta-data"), // cloud metadata
            hit("http://127.0.0.1/admin"),                  // loopback
            hit("http://localhost/"),                       // denied name
            hit("ftp://example.com/x"),                     // non-http scheme
            hit("http://user:pass@example.com/"),           // credentials
        ];
        let (out, dropped) = sanitize_result_urls(results);
        assert_eq!(dropped, 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://ok.example/a");
    }

    #[test]
    fn apply_domain_filter_reports_url_policy_even_without_domain_filters() {
        let results = vec![hit("https://ok.example/a"), hit("http://10.0.0.1/")];
        let (out, reports) = apply_domain_filter(results, &[], &[]);
        assert_eq!(out.len(), 1);
        let url_policy = reports
            .iter()
            .find(|r| r.filter == "result_url_policy")
            .expect("url policy report present");
        assert_eq!(url_policy.enforcement, FilterEnforcement::PostFilter);
        assert!(url_policy.reason.is_some());
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
