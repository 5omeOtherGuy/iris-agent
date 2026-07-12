//! Canonicalization of the `include_domains` / `exclude_domains` filter lists.
//!
//! A caller may type a domain filter in many shapes -- `https://www.Example.com/path`,
//! `*.example.com`, `user@Example.COM:8443`, `bücher.de`, a trailing dot. Before
//! a backend expresses them upstream (`site:`, native params) or post-filters on
//! hostname, we reduce each entry to a bare canonical registrable host:
//!
//! - parse through the `url` crate (WHATWG), which lowercases and IDNA/punycode
//!   -encodes the host and normalizes legacy numeric IPv4 forms, so our
//!   canonical form matches what [`policy`](super::super::policy) and the
//!   connectors see;
//! - strip a leading `*.` wildcard, the scheme, any userinfo/port/path, and a
//!   trailing FQDN-root dot;
//! - drop entries that cannot be a public-domain filter (empty, single-label,
//!   or unparsable), recording a truthful reason;
//! - de-duplicate (order-preserving) and cap each list at [`MAX_DOMAINS`];
//! - reject the whole request when a domain appears in BOTH lists (a domain
//!   cannot be simultaneously required and excluded).
//!
//! Every drop/cap/dedup and the conflict rejection carries a human-readable
//! reason so `metadata.filters` stays honest about what was changed and why.

use std::collections::HashSet;

use url::{Host, Url};

use super::super::{FilterEnforcement, FilterReport};
use super::filters::report_with_reason;

/// Maximum domains honored per list. Extra entries are dropped with a reason so
/// a pathological list cannot blow up the `site:` query or the post-filter.
pub(super) const MAX_DOMAINS: usize = 20;

/// The normalized, deduped, capped filter lists plus the truthful reports for
/// anything that was ignored, capped, or de-duplicated.
#[derive(Debug, Default)]
pub(super) struct NormalizedFilters {
    pub(super) include: Vec<String>,
    pub(super) exclude: Vec<String>,
    pub(super) reports: Vec<FilterReport>,
}

/// Normalize both filter lists. Returns `Err` with an actionable message when a
/// domain is present in both lists after canonicalization (an unsatisfiable
/// include/exclude conflict); the caller surfaces it as the tool error.
pub(super) fn normalize_filters(
    include_raw: &[String],
    exclude_raw: &[String],
) -> Result<NormalizedFilters, String> {
    let mut reports = Vec::new();
    let include = normalize_list("include_domains", include_raw, &mut reports);
    let exclude = normalize_list("exclude_domains", exclude_raw, &mut reports);

    // Conflict: a domain cannot be both required and excluded. Reject the whole
    // request rather than silently preferring one list.
    let exclude_set: HashSet<&String> = exclude.iter().collect();
    let mut conflicts: Vec<&str> = include
        .iter()
        .filter(|d| exclude_set.contains(*d))
        .map(String::as_str)
        .collect();
    if !conflicts.is_empty() {
        conflicts.sort_unstable();
        conflicts.dedup();
        return Err(format!(
            "include_domains and exclude_domains both list {}; a domain cannot be required and \
             excluded at the same time",
            conflicts.join(", ")
        ));
    }

    Ok(NormalizedFilters {
        include,
        exclude,
        reports,
    })
}

/// Canonicalize one list: normalize each entry, drop the unusable ones (with a
/// reason), de-duplicate order-preserving, and cap at [`MAX_DOMAINS`].
fn normalize_list(name: &str, raw: &[String], reports: &mut Vec<FilterReport>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut duplicates = 0usize;
    let mut invalid = 0usize;

    for entry in raw {
        match normalize_domain(entry) {
            Some(domain) => {
                if seen.insert(domain.clone()) {
                    out.push(domain);
                } else {
                    duplicates += 1;
                }
            }
            None => invalid += 1,
        }
    }

    if invalid > 0 {
        reports.push(report_with_reason(
            name,
            FilterEnforcement::PostFilter,
            format!("ignored {invalid} invalid domain(s)"),
        ));
    }

    if duplicates > 0 {
        reports.push(report_with_reason(
            name,
            FilterEnforcement::PostFilter,
            format!("removed {duplicates} duplicate domain(s)"),
        ));
    }

    if out.len() > MAX_DOMAINS {
        let extra = out.len() - MAX_DOMAINS;
        out.truncate(MAX_DOMAINS);
        reports.push(report_with_reason(
            name,
            FilterEnforcement::PostFilter,
            format!("capped to {MAX_DOMAINS} domains; ignored {extra} extra"),
        ));
    }

    out
}

/// Reduce one raw entry to a bare canonical registrable host, or `None` when it
/// cannot be a public-domain filter. Pure and offline: no DNS, no policy IP
/// range checks (the filter only ever matches on hostname; the output URL
/// policy in [`filters`](super::filters) guards actual result URLs).
pub(super) fn normalize_domain(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Strip a leading `*.` wildcard: `*.example.com` filters the same set a
    // suffix-aware match on `example.com` already covers.
    let without_wildcard = trimmed.strip_prefix("*.").unwrap_or(trimmed);

    // Parse through the URL crate so userinfo/port/path/case/IDNA are handled
    // uniformly. Add a scheme when absent so `host:port/path` and `user@host`
    // parse as an authority rather than an opaque path.
    let candidate = if without_wildcard.contains("://") {
        without_wildcard.to_string()
    } else {
        format!("https://{without_wildcard}")
    };
    let url = Url::parse(&candidate).ok()?;

    // `url::Host` gives the canonical host: domains are lowercased and
    // punycode-encoded; IP literals come back as their canonical form.
    let host = match url.host()? {
        Host::Domain(d) => d.to_string(),
        Host::Ipv4(a) => a.to_string(),
        Host::Ipv6(a) => a.to_string(),
    };

    // Strip a trailing FQDN-root dot. Preserve every hostname label: `www` is
    // not semantically interchangeable with its parent domain.
    let host = host.trim_end_matches('.');

    // A usable domain filter needs at least two labels (a registrable host):
    // reject empty, single-label (`intranet`), and empty-label (`a..com`) forms
    // that could never name a public site.
    if host.is_empty() || host.contains("..") || !host.contains('.') {
        return None;
    }

    Some(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_scheme_case_wildcard_port_path_and_userinfo() {
        assert_eq!(
            normalize_domain("https://www.Example.COM/path?q=1"),
            Some("www.example.com".to_string())
        );
        assert_eq!(normalize_domain("*.docs.rs"), Some("docs.rs".to_string()));
        assert_eq!(
            normalize_domain("user:pass@Example.com:8443"),
            Some("example.com".to_string())
        );
        assert_eq!(
            normalize_domain("EXAMPLE.CO.UK."),
            Some("example.co.uk".to_string())
        );
        // A bare host with a path but no scheme still resolves to the host.
        assert_eq!(
            normalize_domain("sub.example.com/a/b"),
            Some("sub.example.com".to_string())
        );
    }

    #[test]
    fn idna_encodes_unicode_hosts_to_punycode() {
        // `bücher.de` -> `xn--bcher-kva.de` (matches what the policy/connector see).
        assert_eq!(
            normalize_domain("bücher.de"),
            Some("xn--bcher-kva.de".to_string())
        );
    }

    #[test]
    fn www_label_is_preserved() {
        assert_eq!(
            normalize_domain("www.example.com"),
            Some("www.example.com".to_string())
        );
        assert_eq!(
            normalize_domain("www.gov.uk"),
            Some("www.gov.uk".to_string())
        );
        assert_eq!(normalize_domain("www.com"), Some("www.com".to_string()));
    }

    #[test]
    fn rejects_unusable_entries() {
        assert_eq!(normalize_domain(""), None);
        assert_eq!(normalize_domain("   "), None);
        assert_eq!(normalize_domain("intranet"), None); // single label
        assert_eq!(normalize_domain("a..com"), None); // empty label
        assert_eq!(normalize_domain("not a domain"), None); // space -> unparsable host
    }

    #[test]
    fn dedupes_and_reports() {
        let mut reports = Vec::new();
        let out = normalize_list(
            "include_domains",
            &[
                "example.com".into(),
                "https://example.com/".into(), // canonicalizes to a duplicate
                "docs.rs".into(),
            ],
            &mut reports,
        );
        assert_eq!(out, vec!["example.com".to_string(), "docs.rs".to_string()]);
        assert!(
            reports.iter().any(|r| r.filter == "include_domains"
                && r.reason.as_deref().unwrap().contains("duplicate"))
        );
    }

    #[test]
    fn caps_at_max_domains_with_a_reason() {
        let mut reports = Vec::new();
        let raw: Vec<String> = (0..MAX_DOMAINS + 5)
            .map(|i| format!("d{i}.example.com"))
            .collect();
        let out = normalize_list("exclude_domains", &raw, &mut reports);
        assert_eq!(out.len(), MAX_DOMAINS);
        assert!(
            reports
                .iter()
                .any(|r| r.reason.as_deref().unwrap().contains("capped"))
        );
    }

    #[test]
    fn ignored_invalid_entries_are_reported() {
        let mut reports = Vec::new();
        let out = normalize_list(
            "include_domains",
            &["good.com".into(), "".into(), "intranet".into()],
            &mut reports,
        );
        assert_eq!(out, vec!["good.com".to_string()]);
        let ignored: Vec<_> = reports
            .iter()
            .filter(|r| r.reason.as_deref().unwrap().contains("invalid domain"))
            .collect();
        assert_eq!(ignored.len(), 1);
        assert!(ignored[0].reason.as_deref().unwrap().contains("2 invalid"));
    }

    #[test]
    fn normalize_filters_rejects_include_exclude_conflict() {
        // Both lists name the same site after canonicalization -> hard error.
        let err = normalize_filters(&["https://Example.com/".into()], &["example.com".into()])
            .unwrap_err();
        assert!(err.contains("example.com"), "message was: {err}");
        assert!(err.contains("cannot be required and excluded"));
    }

    #[test]
    fn normalize_filters_passes_disjoint_lists() {
        let n = normalize_filters(&["docs.rs".into()], &["spam.com".into()]).unwrap();
        assert_eq!(n.include, vec!["docs.rs".to_string()]);
        assert_eq!(n.exclude, vec!["spam.com".to_string()]);
    }
}
