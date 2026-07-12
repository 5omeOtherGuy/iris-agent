//! The two HTTP client profiles (plan §3.1), plus the pinned SSRF-safe fetch
//! walk used by every native (user/model-URL) path.
//!
//! - **Pinned client** — any user/model-supplied URL (native reader, DDG
//!   scrape). Built *fresh per validated hop*: `redirect(none)` (we walk
//!   redirects by hand so each hop is re-gated), `no_proxy()` (a pinned
//!   connection must not be re-routed), `http1_only()` (no cross-host H2
//!   coalescing), and `resolve_to_addrs(host, validated_ips)` so the connection
//!   goes only to the exact IPs [`policy`](super::policy) approved. reqwest
//!   uses the URL port over the override's port (`dns/resolve.rs`), so we pin
//!   with port 0. This closes the DNS-rebinding TOCTOU the reference lives with.
//! - **API client** — the hardcoded Brave/Jina endpoints. A normal shared
//!   client that MAY honor system proxy env, with redirects disabled (auth
//!   headers must never cross an unexpected origin) and the same size/time caps.
//!
//! The security-critical decisions are pure, exhaustively-tested helpers
//! ([`guard_resolved_ips`], [`next_redirect_target`], [`classify_content_type`],
//! [`check_content_encoding`], [`collect_capped`], [`decode_body`]); the async
//! network wiring is thin glue over them. Full round-trips are not unit-tested
//! (the SSRF gate denies the loopback a local test server would need); the gate
//! logic is tested directly instead.

use std::fmt;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;

use futures::stream::{Stream, StreamExt};
use reqwest::header::{CONTENT_ENCODING, CONTENT_TYPE, LOCATION};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::policy::{self, PolicyError};
use super::{CONNECT_TIMEOUT, MAX_BODY_BYTES, MAX_REDIRECTS, TOTAL_DEADLINE};

/// Async DNS seam. The real resolver hits the system resolver; tests inject a
/// canned (and possibly per-call-changing, i.e. rebinding) resolver without any
/// network. A boxed future keeps the trait object-safe without an `async_trait`
/// dependency.
pub(super) trait Resolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send + 'a>>;
}

/// System resolver over `tokio::net::lookup_host`. DNS cancellation is
/// best-effort (the resolver may block in libc under the hood); the total
/// deadline is the hard stop.
pub(super) struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send + 'a>> {
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host, port)).await?;
            Ok(addrs.map(|s| s.ip()).collect())
        })
    }
}

/// Typed fetch failure so callers can render precise, actionable diagnostics
/// (e.g. point a PDF at the Jina reader, or name the SSRF denial).
#[derive(Debug)]
pub(super) enum FetchError {
    /// The URL (or a redirect target) failed the text-level SSRF policy.
    Policy(PolicyError),
    /// A resolved IP is in a denied range (post-DNS / rebinding guard).
    DeniedAddress(IpAddr),
    /// DNS resolution failed or returned no usable address.
    Dns(String),
    /// Too many redirect hops.
    TooManyRedirects,
    /// The response Content-Type is not a text-like type we read (e.g. PDF,
    /// image, octet-stream). Carries the raw type so the caller can name the
    /// Jina alternative.
    UnsupportedContentType(String),
    /// The response used a Content-Encoding we do not decode.
    UnsupportedEncoding(String),
    /// The call was cancelled.
    Cancelled,
    /// The total deadline elapsed.
    Timeout,
    /// Transport/other error.
    Transport(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Policy(e) => write!(f, "{e}"),
            Self::DeniedAddress(ip) => {
                write!(f, "host resolved to a reserved or private address ({ip})")
            }
            Self::Dns(msg) => write!(f, "DNS resolution failed: {msg}"),
            Self::TooManyRedirects => write!(f, "too many redirects (max {MAX_REDIRECTS})"),
            Self::UnsupportedContentType(ct) => {
                write!(f, "unsupported content type for the native reader: {ct}")
            }
            Self::UnsupportedEncoding(enc) => write!(f, "unsupported content encoding: {enc}"),
            Self::Cancelled => write!(f, "request cancelled"),
            Self::Timeout => write!(
                f,
                "request exceeded the {}s deadline",
                TOTAL_DEADLINE.as_secs()
            ),
            Self::Transport(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for FetchError {}

impl From<PolicyError> for FetchError {
    fn from(e: PolicyError) -> Self {
        Self::Policy(e)
    }
}

/// A fetched, decoded page from the pinned path.
#[derive(Debug, Clone)]
pub(super) struct FetchedPage {
    /// Final URL after redirects.
    pub(super) final_url: String,
    /// Final HTTP status.
    pub(super) status: u16,
    /// Whether the body is `text/plain` (the reader passes it through verbatim).
    pub(super) is_plain_text: bool,
    /// Charset-decoded body text (capped).
    pub(super) text: String,
    /// Whether the body hit the size cap.
    pub(super) truncated: bool,
    /// Redirect hops followed.
    pub(super) redirects: u32,
}

/// How a response Content-Type is classified for the native reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContentClass {
    /// HTML/XHTML/XML -> extract to Markdown.
    Markup,
    /// `text/plain` -> verbatim passthrough.
    PlainText,
    /// Small structured text (json / markdown) -> verbatim passthrough.
    StructuredText,
    /// Not readable (PDF, image, binary) -> caller points at Jina.
    Unsupported,
}

/// Fetch a user/model-supplied URL through the pinned, SSRF-safe path: validate
/// -> resolve -> guard every IP -> pinned request (no redirect) -> manual
/// re-gated redirect walk -> content-type gate -> size-capped, charset-decoded
/// body. The whole walk is bounded by the total deadline and abandoned on
/// `cancel`. No auth/cookies are ever attached, so nothing crosses a redirect.
pub(super) async fn fetch_pinned(
    resolver: &dyn Resolver,
    raw_url: &str,
    cancel: &CancellationToken,
) -> Result<FetchedPage, FetchError> {
    let walk = fetch_pinned_inner(resolver, raw_url);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(FetchError::Cancelled),
        r = tokio::time::timeout(TOTAL_DEADLINE, walk) => match r {
            Ok(inner) => inner,
            Err(_) => Err(FetchError::Timeout),
        },
    }
}

async fn fetch_pinned_inner(
    resolver: &dyn Resolver,
    raw_url: &str,
) -> Result<FetchedPage, FetchError> {
    let mut current = raw_url.to_string();
    let mut redirects: u32 = 0;

    loop {
        let validated = policy::validate_external_url(&current)?;
        let url = validated.url.clone();
        let host = validated.host.clone();
        let port = url
            .port_or_known_default()
            .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });

        // Resolve, then guard every answer against the deny tables. Re-run on
        // every hop, so a rebinding resolver that returns a private IP on a
        // later call is still refused here.
        let ips = resolver
            .resolve(&host, port)
            .await
            .map_err(|e| FetchError::Dns(e.to_string()))?;
        guard_resolved_ips(&ips)?;

        let addrs: Vec<SocketAddr> = ips.iter().map(|ip| SocketAddr::new(*ip, 0)).collect();
        let client = build_pinned_client(&host, &addrs)?;

        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| FetchError::Transport(e.to_string()))?;
        let status = response.status();

        if is_followable_redirect(status.as_u16()) {
            if redirects >= MAX_REDIRECTS {
                return Err(FetchError::TooManyRedirects);
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    FetchError::Transport("redirect without a Location header".into())
                })?;
            let next = next_redirect_target(&url, location)?;
            current = next.into();
            redirects += 1;
            continue;
        }

        // Terminal response: gate the content type before reading the body.
        let content_type = header_content_type(response.headers().get(CONTENT_TYPE));
        check_content_encoding(response.headers().get_all(CONTENT_ENCODING).iter())?;
        let class = classify_content_type(&content_type);
        if class == ContentClass::Unsupported {
            let ct = if content_type.is_empty() {
                "application/octet-stream".to_string()
            } else {
                content_type
            };
            return Err(FetchError::UnsupportedContentType(ct));
        }

        let final_url = response.url().to_string();
        // reqwest (gzip feature, no manual Accept-Encoding) auto-decodes gzip and
        // strips the header, so `bytes_stream` yields DECOMPRESSED bytes and the
        // cap stays meaningful against a gzip bomb.
        let (bytes, truncated) = collect_capped(
            response.bytes_stream(),
            MAX_BODY_BYTES,
            &CancellationToken::new(),
        )
        .await?;
        let text = decode_body(&bytes, &content_type);

        return Ok(FetchedPage {
            final_url,
            status: status.as_u16(),
            is_plain_text: class == ContentClass::PlainText,
            text,
            truncated,
            redirects,
        });
    }
}

/// A resolver that refuses every name. Installed on the pinned client BENEATH
/// the `resolve_to_addrs` override: an exact host match uses the pinned IPs, and
/// any other name (e.g. a trailing-dot host that missed the override key, or a
/// stray lookup) fails closed instead of hitting the system resolver. This is
/// the defense-in-depth half of the DNS-rebinding fix; host normalization in
/// [`policy`](super::policy) is the other half.
struct NoFallbackResolver;

impl reqwest::dns::Resolve for NoFallbackResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let err: Box<dyn std::error::Error + Send + Sync> =
                format!("pinned client refuses DNS fallback for {host:?} (override miss)").into();
            Err(err)
        })
    }
}

/// Build a fresh pinned client for one hop: no redirects (we walk them), no
/// proxy, HTTP/1 only, DNS overridden to the exact validated IPs with a
/// fail-closed resolver beneath it. TLS SNI/cert validation still uses the
/// hostname. Never reused across hops.
fn build_pinned_client(host: &str, addrs: &[SocketAddr]) -> Result<reqwest::Client, FetchError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .http1_only()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_DEADLINE)
        .user_agent(super::user_agent())
        .dns_resolver(std::sync::Arc::new(NoFallbackResolver))
        .resolve_to_addrs(host, addrs)
        .build()
        .map_err(|e| FetchError::Transport(format!("failed to build client: {e}")))
}

/// The shared API client for the hardcoded Brave/Jina endpoints: redirects
/// disabled (auth must not cross origins), system proxy allowed, same deadline.
pub(super) fn build_api_client() -> Result<reqwest::Client, FetchError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_DEADLINE)
        .user_agent(super::user_agent())
        .build()
        .map_err(|e| FetchError::Transport(format!("failed to build API client: {e}")))
}

/// Send a prepared API request and return `(status, body_bytes, truncated)`.
/// Bounds the response to `cap` bytes before the caller parses it (JSON for the
/// search backends, Markdown for the Jina reader), and races the whole thing
/// against `cancel` + the total deadline.
pub(super) async fn send_api(
    request: reqwest::RequestBuilder,
    cap: usize,
    cancel: &CancellationToken,
) -> Result<(u16, Vec<u8>, bool), FetchError> {
    let fut = async {
        let response = request
            .send()
            .await
            .map_err(|e| FetchError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let (bytes, truncated) =
            collect_capped(response.bytes_stream(), cap, &CancellationToken::new()).await?;
        Ok::<_, FetchError>((status, bytes, truncated))
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(FetchError::Cancelled),
        r = tokio::time::timeout(TOTAL_DEADLINE, fut) => match r {
            Ok(inner) => inner,
            Err(_) => Err(FetchError::Timeout),
        },
    }
}

// ---------------------------------------------------------------------------
// Pure, exhaustively-tested security helpers.
// ---------------------------------------------------------------------------

/// The redirect status codes that carry a `Location` and we follow: moved
/// permanently/found/see-other/temporary/permanent. 300/304/305/306 are
/// deliberately excluded (no single Location to walk).
fn is_followable_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Reject the whole answer set if ANY resolved address is denied. All-or-none:
/// a name that resolves to a mix of public and private addresses is refused, so
/// a later connection can never race onto the private one.
pub(super) fn guard_resolved_ips(ips: &[IpAddr]) -> Result<(), FetchError> {
    if ips.is_empty() {
        return Err(FetchError::Dns("no addresses resolved".into()));
    }
    for ip in ips {
        if policy::ip_is_denied(ip) {
            return Err(FetchError::DeniedAddress(*ip));
        }
    }
    Ok(())
}

/// Resolve a redirect `Location` (absolute or relative) against the current URL
/// and strip any fragment, then hand it back for a full re-gate by the caller.
/// The returned URL is NOT yet policy-checked here — the walk re-validates it.
pub(super) fn next_redirect_target(base: &Url, location: &str) -> Result<Url, FetchError> {
    let location = location.trim();
    if location.is_empty() {
        return Err(FetchError::Transport("empty redirect Location".into()));
    }
    // Cap an absurd Location to avoid pathological inputs.
    if location.len() > 4096 {
        return Err(FetchError::Transport("redirect Location too long".into()));
    }
    let mut next = base
        .join(location)
        .map_err(|_| FetchError::Transport("invalid redirect Location".into()))?;
    next.set_fragment(None);
    Ok(next)
}

/// Reject any Content-Encoding the pinned path does not transparently decode.
/// reqwest's gzip feature decodes gzip and removes the header, so any coding
/// still present here is one we did not ask for (br/deflate/zstd). Inspects
/// EVERY `Content-Encoding` header and every comma-separated coding, so a
/// stacked or repeated encoding cannot slip a non-identity coding past the cap.
pub(super) fn check_content_encoding<'a>(
    values: impl IntoIterator<Item = &'a reqwest::header::HeaderValue>,
) -> Result<(), FetchError> {
    for value in values {
        let Ok(text) = value.to_str() else {
            return Err(FetchError::UnsupportedEncoding("<non-ascii>".into()));
        };
        for coding in text.split(',') {
            let coding = coding.trim().to_ascii_lowercase();
            if !coding.is_empty() && coding != "identity" {
                return Err(FetchError::UnsupportedEncoding(coding));
            }
        }
    }
    Ok(())
}

/// Extract a lowercased, param-stripped Content-Type from the header.
fn header_content_type(header: Option<&reqwest::header::HeaderValue>) -> String {
    header
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default()
}

/// Classify a (lowercased, param-stripped) Content-Type for the native reader.
/// An empty/missing type is treated as markup (many servers omit it on HTML).
pub(super) fn classify_content_type(content_type: &str) -> ContentClass {
    let ct = content_type.trim();
    if ct.is_empty()
        || ct == "text/html"
        || ct == "application/xhtml+xml"
        || ct == "text/xml"
        || ct == "application/xml"
    {
        return ContentClass::Markup;
    }
    if ct == "text/plain" {
        return ContentClass::PlainText;
    }
    if ct == "application/json"
        || ct.ends_with("+json")
        || ct == "text/markdown"
        || ct == "text/x-markdown"
    {
        return ContentClass::StructuredText;
    }
    ContentClass::Unsupported
}

/// Charset-decode a capped byte buffer using the Content-Type charset when
/// present, defaulting to UTF-8. `encoding_rs::decode` never fails (it uses
/// replacement characters), so a mislabeled body still yields text.
pub(super) fn decode_body(bytes: &[u8], content_type: &str) -> String {
    let charset = content_type_charset(content_type);
    let encoding = charset
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(bytes);
    text.into_owned()
}

/// Pull the `charset=` parameter out of a raw Content-Type value.
fn content_type_charset(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|param| {
        let (k, v) = param.split_once('=')?;
        if k.trim().eq_ignore_ascii_case("charset") {
            Some(v.trim().trim_matches('"').to_ascii_lowercase())
        } else {
            None
        }
    })
}

/// Accumulate a byte stream up to `cap` DECOMPRESSED bytes, stopping (and
/// flagging truncation) once the cap is hit, and abandoning on `cancel`. Generic
/// over the chunk type so both reqwest's `bytes_stream` and in-memory test
/// streams drive it.
pub(super) async fn collect_capped<S, B, E>(
    stream: S,
    cap: usize,
    cancel: &CancellationToken,
) -> Result<(Vec<u8>, bool), FetchError>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: fmt::Display,
{
    futures::pin_mut!(stream);
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(FetchError::Cancelled),
            chunk = stream.next() => {
                let Some(chunk) = chunk else { break };
                let chunk = chunk.map_err(|e| FetchError::Transport(e.to_string()))?;
                let chunk = chunk.as_ref();
                if buf.len() + chunk.len() > cap {
                    let take = cap.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                    truncated = true;
                    break;
                }
                buf.extend_from_slice(chunk);
            }
        }
    }
    Ok((buf, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn guard_rejects_any_private_answer() {
        let ok = ["93.184.216.34".parse().unwrap(), "8.8.8.8".parse().unwrap()];
        assert!(guard_resolved_ips(&ok).is_ok());

        // Mixed public + private is refused entirely (no racing onto the private one).
        let mixed = [
            "93.184.216.34".parse::<IpAddr>().unwrap(),
            "127.0.0.1".parse().unwrap(),
        ];
        assert!(matches!(
            guard_resolved_ips(&mixed),
            Err(FetchError::DeniedAddress(_))
        ));

        // Rebinding: simulate a second-call answer that flipped to metadata.
        let rebind = ["169.254.169.254".parse().unwrap()];
        assert!(matches!(
            guard_resolved_ips(&rebind),
            Err(FetchError::DeniedAddress(_))
        ));

        assert!(matches!(guard_resolved_ips(&[]), Err(FetchError::Dns(_))));
    }

    #[test]
    fn redirect_to_private_is_caught_by_revalidation() {
        // next_redirect_target only resolves; the walk re-gates. Prove the
        // resolved target is then rejected by the policy.
        let base = Url::parse("https://example.com/a").unwrap();
        let target =
            next_redirect_target(&base, "http://169.254.169.254/latest/meta-data").unwrap();
        assert!(policy::validate_external_url(target.as_str()).is_err());
    }

    #[test]
    fn relative_redirect_resolves_against_base_and_strips_fragment() {
        let base = Url::parse("https://example.com/dir/page?x=1").unwrap();
        let next = next_redirect_target(&base, "/other#frag").unwrap();
        assert_eq!(next.as_str(), "https://example.com/other");
        let rel = next_redirect_target(&base, "sibling").unwrap();
        assert_eq!(rel.as_str(), "https://example.com/dir/sibling");
    }

    #[test]
    fn redirect_target_rejects_empty_and_oversized() {
        let base = Url::parse("https://example.com/").unwrap();
        assert!(next_redirect_target(&base, "   ").is_err());
        let huge = "https://example.com/".to_string() + &"a".repeat(5000);
        assert!(next_redirect_target(&base, &huge).is_err());
    }

    #[test]
    fn content_encoding_gate() {
        let ok: [&HeaderValue; 0] = [];
        assert!(check_content_encoding(ok.iter().copied()).is_ok());
        assert!(check_content_encoding([&HeaderValue::from_static("identity")]).is_ok());
        assert!(check_content_encoding([&HeaderValue::from_static("")]).is_ok());
        assert!(matches!(
            check_content_encoding([&HeaderValue::from_static("br")]),
            Err(FetchError::UnsupportedEncoding(_))
        ));
        assert!(matches!(
            check_content_encoding([&HeaderValue::from_static("zstd")]),
            Err(FetchError::UnsupportedEncoding(_))
        ));
        // Stacked codings: gzip already stripped by reqwest, but a trailing br
        // in a comma list must still be rejected.
        assert!(matches!(
            check_content_encoding([&HeaderValue::from_static("identity, br")]),
            Err(FetchError::UnsupportedEncoding(_))
        ));
        // Repeated headers.
        assert!(matches!(
            check_content_encoding([
                &HeaderValue::from_static("identity"),
                &HeaderValue::from_static("deflate"),
            ]),
            Err(FetchError::UnsupportedEncoding(_))
        ));
    }

    #[test]
    fn content_type_classification() {
        use ContentClass::*;
        assert_eq!(classify_content_type(""), Markup);
        assert_eq!(classify_content_type("text/html"), Markup);
        assert_eq!(classify_content_type("application/xhtml+xml"), Markup);
        assert_eq!(classify_content_type("text/plain"), PlainText);
        assert_eq!(classify_content_type("application/json"), StructuredText);
        assert_eq!(classify_content_type("application/ld+json"), StructuredText);
        assert_eq!(classify_content_type("text/markdown"), StructuredText);
        assert_eq!(classify_content_type("application/pdf"), Unsupported);
        assert_eq!(classify_content_type("image/png"), Unsupported);
        assert_eq!(
            classify_content_type("application/octet-stream"),
            Unsupported
        );
    }

    #[test]
    fn header_content_type_strips_params() {
        let hv = HeaderValue::from_static("text/HTML; charset=UTF-8");
        assert_eq!(header_content_type(Some(&hv)), "text/html");
    }

    #[test]
    fn decode_body_honors_charset_and_defaults_utf8() {
        // UTF-8 default: raw bytes 0xC3 0xA9 are the UTF-8 encoding of é.
        assert_eq!(
            decode_body(&[b'c', b'a', b'f', 0xc3, 0xa9], "text/html"),
            "caf\u{e9}"
        );
        // Latin-1 label decodes 0xE9 -> é.
        assert_eq!(
            decode_body(&[b'c', b'a', b'f', 0xe9], "text/html; charset=iso-8859-1"),
            "caf\u{e9}"
        );
    }

    #[tokio::test]
    async fn collect_capped_enforces_the_cap_against_a_bomb() {
        // Two 4 KiB chunks with a 6 KiB cap -> exactly 6 KiB, truncated.
        let chunks: Vec<Result<Vec<u8>, String>> = vec![Ok(vec![b'x'; 4096]), Ok(vec![b'y'; 4096])];
        let stream = futures::stream::iter(chunks);
        let (buf, truncated) = collect_capped(stream, 6144, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(buf.len(), 6144);
        assert!(truncated);
    }

    #[tokio::test]
    async fn collect_capped_passes_small_bodies_whole() {
        let chunks: Vec<Result<Vec<u8>, String>> =
            vec![Ok(b"hello ".to_vec()), Ok(b"world".to_vec())];
        let stream = futures::stream::iter(chunks);
        let (buf, truncated) = collect_capped(stream, 1024, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(buf, b"hello world");
        assert!(!truncated);
    }

    #[tokio::test]
    async fn collect_capped_is_cancellable() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let chunks: Vec<Result<Vec<u8>, String>> = vec![Ok(vec![b'x'; 10])];
        let stream = futures::stream::iter(chunks);
        assert!(matches!(
            collect_capped(stream, 1024, &cancel).await,
            Err(FetchError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn collect_capped_surfaces_stream_errors() {
        let chunks: Vec<Result<Vec<u8>, String>> = vec![Err("mid-body reset".into())];
        let stream = futures::stream::iter(chunks);
        assert!(matches!(
            collect_capped(stream, 1024, &CancellationToken::new()).await,
            Err(FetchError::Transport(_))
        ));
    }

    /// A rebinding resolver: public IP on the first call, private on the second.
    /// Demonstrates the resolver seam; the per-hop `guard_resolved_ips` is what
    /// defeats it in the real walk.
    struct RebindResolver {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Resolver for RebindResolver {
        fn resolve<'a>(
            &'a self,
            _host: &'a str,
            _port: u16,
        ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send + 'a>> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    Ok(vec!["93.184.216.34".parse().unwrap()])
                } else {
                    Ok(vec!["127.0.0.1".parse().unwrap()])
                }
            })
        }
    }

    #[tokio::test]
    async fn rebinding_resolver_second_answer_is_guarded() {
        let r = RebindResolver {
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let first = r.resolve("example.com", 443).await.unwrap();
        assert!(guard_resolved_ips(&first).is_ok());
        let second = r.resolve("example.com", 443).await.unwrap();
        assert!(matches!(
            guard_resolved_ips(&second),
            Err(FetchError::DeniedAddress(_))
        ));
    }
}
