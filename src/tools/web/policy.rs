//! The single URL/IP SSRF policy gate for both web tools.
//!
//! Ported from ampi-web's `url-policy.ts`, then hardened: the deny tables are
//! built from the IANA special-purpose-address registries (IPv4 + IPv6), not
//! the reference's shorter list. Two entry points:
//!
//! - [`validate_external_url`] gates a user/model-supplied URL *by text*:
//!   scheme, port, userinfo, canonical host, denied names, and — when the host
//!   is an IP literal — the deny tables. Runs before any DNS.
//! - [`ip_is_denied`] range-checks a *resolved* address. [`fetch`](super::fetch)
//!   calls it on every DNS answer and on every redirect hop, closing the
//!   DNS-rebinding window that text-only validation cannot (the reference
//!   documents this residual TOCTOU; we do not have it).
//!
//! The `url` crate does WHATWG parsing, so it canonicalizes IDNA/punycode
//! hosts, lowercases domains, and normalizes legacy numeric IPv4 forms
//! (`0x7f.1`, `0177.0.0.1`, `2130706433`) into an [`Ipv4Addr`] before we range
//! -check them — a class of bypass the reference's string checks would miss.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// Hostnames that must never resolve to a public target, independent of DNS.
const DENIED_HOSTNAMES: &[&str] = &[
    "localhost",
    "ip6-localhost",
    "ip6-loopback",
    "broadcasthost",
];

/// Domain suffixes that name local/internal services by convention.
const DENIED_SUFFIXES: &[&str] = &[".local", ".localhost", ".internal"];

/// A URL that passed the text-level policy. Carries the parsed [`Url`] and the
/// derived canonical host (lowercased, trailing-dot stripped, punycode ASCII)
/// so callers reuse the same host string the gate approved (e.g. suffix-aware
/// domain filters in `web_search`).
#[derive(Debug, Clone)]
pub(super) struct ValidatedUrl {
    pub(super) url: Url,
    pub(super) host: String,
}

/// Why a URL was rejected. A plain reason string keeps the message actionable
/// at the tool boundary while staying cheap to assert on in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PolicyError {
    pub(super) reason: String,
}

impl PolicyError {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for PolicyError {}

/// Validate a user/model-supplied URL against the SSRF policy, by text only.
///
/// Enforces: absolute http/https, ports 80/443 only, no userinfo credentials,
/// a present canonical host, no denied name or `.local`/`.localhost`/`.internal`
/// suffix, no bare single-label host, and — for IP-literal hosts — the deny
/// tables. Returns the canonicalized host alongside the parsed URL. This does
/// NOT resolve DNS; [`ip_is_denied`] guards the resolved addresses.
pub(super) fn validate_external_url(raw: &str) -> Result<ValidatedUrl, PolicyError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(PolicyError::new("URL is empty."));
    }
    let mut url =
        Url::parse(raw).map_err(|_| PolicyError::new("URL is not a valid absolute URL."))?;

    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(PolicyError::new(format!(
                "URL scheme {other:?} is not allowed; only http and https are supported."
            )));
        }
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(PolicyError::new(
            "URL must not include credentials in the userinfo component.",
        ));
    }

    // `Url::port` returns `None` for the scheme-default port, so any `Some`
    // here is a deliberate non-default port. Allow only 80/443 (per plan §6.1;
    // 8080/8443 were cut and re-add only with a demonstrated need).
    if let Some(port) = url.port()
        && port != 80
        && port != 443
    {
        return Err(PolicyError::new(format!(
            "URL port {port} is not allowed; only the default http/https ports (80, 443) are permitted."
        )));
    }

    // Take an OWNED copy of the host classification so the immutable borrow of
    // `url` ends here and the domain branch can normalize `url` in place.
    enum HostKind {
        V4(Ipv4Addr),
        V6(Ipv6Addr),
        Domain(String),
    }
    let kind = match url.host() {
        None => return Err(PolicyError::new("URL has no hostname.")),
        Some(Host::Ipv4(a)) => HostKind::V4(a),
        Some(Host::Ipv6(a)) => HostKind::V6(a),
        Some(Host::Domain(d)) => HostKind::Domain(d.to_string()),
    };

    match kind {
        HostKind::V4(addr) => {
            if ipv4_is_denied(&addr) {
                return Err(PolicyError::new(format!(
                    "IP {addr} is in a reserved or private range."
                )));
            }
            let host = addr.to_string();
            Ok(ValidatedUrl { url, host })
        }
        HostKind::V6(addr) => {
            if ipv6_is_denied(&addr) {
                return Err(PolicyError::new(format!(
                    "IPv6 {addr} is in a reserved or private range."
                )));
            }
            let host = addr.to_string();
            Ok(ValidatedUrl { url, host })
        }
        HostKind::Domain(domain) => {
            // `url` already lowercases and IDNA-encodes the domain. Strip a
            // single FQDN-root trailing dot so `localhost.` cannot bypass the
            // name checks, then apply the name policy.
            // Empty labels (`a..com`, `host..`, a double trailing dot) are
            // malformed and could canonicalize inconsistently between our
            // checks and the connector -- inspect the ORIGINAL domain before we
            // strip the single legal FQDN-root dot.
            if domain.contains("..") {
                return Err(PolicyError::new(format!(
                    "Hostname {domain:?} has an empty label."
                )));
            }
            let host = domain.strip_suffix('.').unwrap_or(&domain).to_string();
            if host.is_empty() {
                return Err(PolicyError::new("URL has no hostname."));
            }
            if DENIED_HOSTNAMES.contains(&host.as_str()) {
                return Err(PolicyError::new(format!(
                    "Hostname {host:?} is not allowed."
                )));
            }
            if DENIED_SUFFIXES.iter().any(|suffix| host.ends_with(suffix)) {
                return Err(PolicyError::new(format!(
                    "Hostname {host:?} looks like a local/internal name."
                )));
            }
            // A public target is always a multi-label FQDN. Reject a bare
            // single label (`http://intranet/`) -- it can only name a
            // search-domain-completed internal host.
            if !host.contains('.') {
                return Err(PolicyError::new(format!(
                    "Hostname {host:?} is a single label; a public host needs a domain."
                )));
            }
            // BLOCKER FIX (SSRF): normalize the URL host to the canonical form
            // so a later `resolve_to_addrs(host, ...)` DNS pin keys on EXACTLY
            // the request host. Without this, a trailing-dot host (`host.`)
            // whose canonical form is `host` would miss the pin and let reqwest
            // fall back to real DNS -- reopening the rebinding window.
            if url.host_str() != Some(host.as_str()) {
                url.set_host(Some(&host))
                    .map_err(|_| PolicyError::new("Hostname could not be normalized."))?;
            }
            debug_assert_eq!(url.host_str(), Some(host.as_str()));
            Ok(ValidatedUrl { url, host })
        }
    }
}

/// Range-check a resolved address against the deny tables. Called on every DNS
/// answer and every redirect hop so a name that passed text validation but
/// resolves (or re-resolves) to a private/reserved address is still refused.
pub(super) fn ip_is_denied(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_denied(v4),
        IpAddr::V6(v6) => ipv6_is_denied(v6),
    }
}

/// IANA IPv4 Special-Purpose Address Registry (deny everything not routable on
/// the public internet). Kept explicit rather than relying on the mix of
/// stable/unstable `Ipv4Addr` predicates so the table is auditable and stable.
fn ipv4_is_denied(addr: &Ipv4Addr) -> bool {
    let o = addr.octets();
    o[0] == 0                                        // 0.0.0.0/8    this host
        || o[0] == 10                                // 10.0.0.0/8   private
        || (o[0] == 100 && (o[1] & 0xc0) == 0x40)    // 100.64/10    CGNAT shared
        || o[0] == 127                               // 127.0.0.0/8  loopback
        || (o[0] == 169 && o[1] == 254)              // 169.254/16   link-local
        || (o[0] == 172 && (o[1] & 0xf0) == 0x10)    // 172.16/12    private
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)   // 192.0.0/24   IETF protocol
        || (o[0] == 192 && o[1] == 0 && o[2] == 2)   // 192.0.2/24   TEST-NET-1
        || (o[0] == 192 && o[1] == 88 && o[2] == 99) // 192.88.99/24 6to4 relay anycast
        || (o[0] == 192 && o[1] == 168)              // 192.168/16   private
        || (o[0] == 198 && (o[1] & 0xfe) == 18)      // 198.18/15    benchmarking
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)// 198.51.100/24 TEST-NET-2
        || (o[0] == 203 && o[1] == 0 && o[2] == 113) // 203.0.113/24 TEST-NET-3
        || o[0] >= 224 // 224/4 multicast + 240/4 reserved (incl. 255.255.255.255)
}

/// IANA IPv6 Special-Purpose Address Registry, plus recursion into IPv4 tails
/// of transition mechanisms (mapped/NAT64/6to4) so a private IPv4 cannot be
/// smuggled through an IPv6 wrapper.
fn ipv6_is_denied(addr: &Ipv6Addr) -> bool {
    let s = addr.segments();
    let b = addr.octets();

    if addr.is_unspecified() || addr.is_loopback() {
        return true; // ::/128, ::1/128
    }
    if b[0] == 0xff {
        return true; // ff00::/8 multicast
    }
    if (b[0] & 0xfe) == 0xfc {
        return true; // fc00::/7 unique local
    }
    if b[0] == 0xfe && (b[1] & 0xc0) == 0x80 {
        return true; // fe80::/10 link-local
    }
    // 100::/64 discard-only, and 100:0:0:1::/64 dummy-address prefix.
    if s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && (s[3] == 0 || s[3] == 1) {
        return true;
    }
    // 2001::/23 IETF protocol assignments (covers 2001::/32 Teredo and the
    // benchmarking/ORCHID sub-blocks); 2001:db8::/32 documentation sits outside
    // /23 so it stays explicit; 3fff::/20 documentation; 5f00::/16 SRv6 SIDs.
    if s[0] == 0x2001 && s[1] < 0x0200 {
        return true;
    }
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return true;
    }
    if (s[0] & 0xfff0) == 0x3ff0 {
        return true;
    }
    if s[0] == 0x5f00 {
        return true;
    }
    // ::ffff:0:0/96 IPv4-mapped -> deny the ENTIRE block. IPv4-mapped IPv6
    // literals in a URL host are an SSRF-obfuscation vector with no legitimate
    // public use, so we reject all of them, not just private IPv4 tails.
    if s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0xffff {
        return true;
    }
    // 64:ff9b::/96 (well-known NAT64) and 64:ff9b:1::/48 (local-use NAT64) are
    // special-purpose transition ranges: deny the WHOLE blocks regardless of
    // the embedded IPv4 tail. They route through a translator, not ordinary
    // public egress, so even a public-looking tail must not be reachable.
    if s[0] == 0x0064 && s[1] == 0xff9b {
        return true;
    }
    // 2002::/16 (6to4, deprecated per RFC 7526): deny the whole block, not just
    // 6to4 wrappers of private IPv4 -- the relay path is not public egress.
    if s[0] == 0x2002 {
        return true;
    }
    false
}

/// Suffix-aware host match for domain filters (`include_domains`/
/// `exclude_domains`): a host matches `domain` when it equals it or is a
/// subdomain of it. Both sides are compared on the canonical (lowercased,
/// trailing-dot-stripped) form. Used by `web_search`'s filter layer.
pub(super) fn host_matches_domain(host: &str, domain: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let domain = domain
        .trim_end_matches('.')
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if domain.is_empty() {
        return false;
    }
    host == domain || host.ends_with(&format!(".{domain}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(raw: &str) -> String {
        validate_external_url(raw).unwrap_err().reason
    }

    #[test]
    fn accepts_ordinary_public_urls() {
        let v = validate_external_url("https://Example.COM/path?q=1#frag").unwrap();
        assert_eq!(v.host, "example.com");
        assert_eq!(v.url.scheme(), "https");
        assert!(validate_external_url("http://sub.example.co.uk:80/").is_ok());
        assert!(validate_external_url("https://example.com:443/").is_ok());
    }

    #[test]
    fn rejects_non_http_schemes() {
        assert!(reason("file:///etc/passwd").contains("scheme"));
        assert!(reason("ftp://example.com/").contains("scheme"));
        assert!(
            reason("javascript:alert(1)").contains("not allowed")
                || reason("javascript:alert(1)").contains("scheme")
                || reason("javascript:alert(1)").contains("valid")
        );
    }

    #[test]
    fn rejects_userinfo_and_bad_ports() {
        assert!(reason("http://user:pass@example.com/").contains("userinfo"));
        assert!(reason("http://user@example.com/").contains("userinfo"));
        assert!(reason("http://example.com:8080/").contains("port"));
        assert!(reason("https://example.com:22/").contains("port"));
    }

    #[test]
    fn rejects_denied_names_and_suffixes() {
        assert!(reason("http://localhost/").contains("not allowed"));
        assert!(reason("http://localhost./").contains("not allowed")); // trailing-dot bypass
        assert!(reason("http://foo.local/").contains("local/internal"));
        assert!(reason("http://svc.internal/").contains("local/internal"));
        assert!(reason("http://api.localhost/").contains("local/internal"));
    }

    #[test]
    fn rejects_single_label_hosts() {
        assert!(reason("http://intranet/").contains("single label"));
    }

    #[test]
    fn rejects_private_ipv4_including_legacy_forms() {
        for raw in [
            "http://127.0.0.1/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://169.254.169.254/", // cloud metadata
            "http://100.100.0.1/",     // CGNAT
            "http://0.0.0.0/",
            "http://192.0.2.5/",    // TEST-NET-1
            "http://198.51.100.9/", // TEST-NET-2
            "http://203.0.113.9/",  // TEST-NET-3
            "http://198.18.0.1/",   // benchmarking
            "http://255.255.255.255/",
            "http://224.0.0.1/",
        ] {
            assert!(
                reason(raw).contains("reserved or private"),
                "expected {raw} denied"
            );
        }
    }

    #[test]
    fn rejects_legacy_numeric_ipv4_forms() {
        // WHATWG parsing normalizes these to 127.0.0.1, which the deny table
        // catches. If a form is rejected as an invalid URL instead, that is
        // also a safe outcome; assert it never validates OK.
        for raw in [
            "http://2130706433/", // decimal 127.0.0.1
            "http://0x7f.0.0.1/", // hex first octet
            "http://0177.0.0.1/", // octal first octet
            "http://127.1/",      // short form -> 127.0.0.1
        ] {
            assert!(
                validate_external_url(raw).is_err(),
                "expected {raw} to be rejected"
            );
        }
    }

    #[test]
    fn allows_public_ipv4() {
        assert!(validate_external_url("http://93.184.216.34/").is_ok()); // example.com
        assert!(validate_external_url("http://8.8.8.8/").is_ok());
    }

    #[test]
    fn rejects_private_ipv6_and_wrappers() {
        for raw in [
            "http://[::1]/",                    // loopback
            "http://[::]/",                     // unspecified
            "http://[fe80::1]/",                // link-local
            "http://[fc00::1]/",                // unique local
            "http://[fd00::1]/",                // unique local
            "http://[ff02::1]/",                // multicast
            "http://[2001:db8::1]/",            // documentation
            "http://[::ffff:127.0.0.1]/",       // v4-mapped loopback
            "http://[::ffff:169.254.169.254]/", // v4-mapped metadata
            "http://[2001::1]/",                // 2001::/23 protocol (Teredo)
            "http://[5f00::1]/",                // SRv6 SIDs
            "http://[100:0:0:1::1]/",           // dummy-address prefix
            "http://[::ffff:8.8.8.8]/",         // v4-mapped PUBLIC v4 still denied
            "http://[64:ff9b::7f00:1]/",        // NAT64 of 127.0.0.1
            "http://[64:ff9b::808:808]/",       // NAT64 of PUBLIC 8.8.8.8 -> still denied
            "http://[2002:7f00:1::]/",          // 6to4 of 127.0.0.1
            "http://[2002:808:808::]/",         // 6to4 of PUBLIC 8.8.8.8 -> still denied
            "http://[100::1]/",                 // discard-only
        ] {
            assert!(
                reason(raw).contains("reserved or private"),
                "expected {raw} denied"
            );
        }
    }

    #[test]
    fn allows_public_ipv6() {
        assert!(validate_external_url("http://[2606:4700:4700::1111]/").is_ok()); // 1.1.1.1
    }

    #[test]
    fn normalizes_trailing_dot_host_so_the_pin_cannot_miss() {
        // BLOCKER regression: a trailing-dot host must canonicalize so the URL
        // host equals the returned host (otherwise the DNS pin key misses and
        // reqwest falls back to real DNS).
        let v = validate_external_url("https://Example.COM./path").unwrap();
        assert_eq!(v.host, "example.com");
        assert_eq!(v.url.host_str(), Some("example.com"));
        // Empty interior/adjacent labels are rejected outright.
        assert!(validate_external_url("https://a.com../").is_err());
        // A trailing-dot denied name is still denied.
        assert!(validate_external_url("http://localhost./").is_err());
    }

    #[test]
    fn ip_is_denied_covers_resolved_addresses() {
        assert!(ip_is_denied(&"127.0.0.1".parse().unwrap()));
        assert!(ip_is_denied(&"169.254.169.254".parse().unwrap()));
        assert!(ip_is_denied(&"::1".parse().unwrap()));
        assert!(ip_is_denied(&"fd12::1".parse().unwrap()));
        assert!(!ip_is_denied(&"8.8.8.8".parse().unwrap()));
        assert!(!ip_is_denied(&"2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn host_matches_domain_is_suffix_aware() {
        assert!(host_matches_domain("example.com", "example.com"));
        assert!(host_matches_domain("www.example.com", "example.com"));
        assert!(host_matches_domain("a.b.example.com", "example.com"));
        assert!(host_matches_domain("example.com.", "example.com")); // trailing dot
        assert!(host_matches_domain("www.example.com", ".example.com")); // leading dot in filter
        assert!(!host_matches_domain("notexample.com", "example.com"));
        assert!(!host_matches_domain("example.com.evil.com", "example.com"));
        assert!(!host_matches_domain("example.com", ""));
    }
}
