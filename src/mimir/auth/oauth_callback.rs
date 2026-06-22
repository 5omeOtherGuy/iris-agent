//! Shared, Mimir-local OAuth browser-login plumbing (Tier 3): PKCE S256, a
//! cancel-aware and timeout-bounded local callback server bound on the dual-stack
//! loopback, callback-request classification, manual authorization-input parsing,
//! and the HTTP callback response. The openai_codex, antigravity, and anthropic
//! browser-login flows share this so each provider owns only its wire format
//! (authorize URL, token exchange), never the listener/cancellation glue.
//!
//! Cancellation + timeout: the listeners are non-blocking and polled in a short
//! loop that checks the [`CancellationToken`] and an overall deadline between
//! iterations, so a TUI cancel releases the callback port within one poll tick
//! instead of parking forever in `TcpListener::incoming()`.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use reqwest::Url;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

/// How long a browser login waits for the callback before timing out. Matches
/// the per-login HTTP client timeout used by the callers.
pub(crate) const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Poll cadence for the non-blocking accept loop: small enough that a TUI cancel
/// or a pasted code is acted on promptly, large enough not to busy-spin.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Read timeout for a single accepted callback connection, so a half-open
/// connection cannot wedge the accept loop.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// A PKCE S256 verifier/challenge pair.
#[derive(Debug, Clone)]
pub(crate) struct Pkce {
    pub(crate) verifier: String,
    pub(crate) challenge: String,
}

/// Create a fresh PKCE pair: a random URL-safe verifier and its base64url
/// SHA-256 challenge.
pub(crate) fn create_pkce() -> Pkce {
    let verifier = random_url_token(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// A random URL-safe base64 token of `byte_len` random bytes (used for PKCE
/// verifiers and OAuth `state`).
pub(crate) fn random_url_token(byte_len: usize) -> String {
    let mut bytes = vec![0_u8; byte_len];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Classification of one inbound HTTP request to the callback port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallbackOutcome {
    /// A valid authorization code whose `state` matched the expected value.
    Code(String),
    /// The provider redirected with an `error=` parameter (terminal failure).
    OAuthError(String),
    /// A `code` was present but `state` did not match: reject without accepting.
    StateMismatch,
    /// The request hit a path other than the callback path.
    WrongPath,
    /// No code (and no error) was present (e.g. a favicon probe).
    Missing,
}

/// Per-login callback parameters.
pub(crate) struct CallbackConfig<'a> {
    pub(crate) expected_state: &'a str,
    pub(crate) callback_path: &'a str,
    pub(crate) success_message: &'a str,
    /// Short provider label used only in user-facing progress/error text.
    pub(crate) provider_label: &'a str,
}

/// A local OAuth callback server bound on the loopback interface.
#[derive(Debug)]
pub(crate) struct CallbackServer {
    listeners: Vec<TcpListener>,
    port: u16,
}

impl CallbackServer {
    /// Bind the callback `port` on IPv4 loopback (always) and IPv6 loopback
    /// (best-effort). The registered redirect URIs use `localhost`, which can
    /// resolve to either `127.0.0.1` or `::1` depending on the host, so both are
    /// bound where supported to keep the browser callback reachable. An
    /// `AddrInUse` failure on the IPv4 bind yields an actionable error naming the
    /// port and the likely cause (a previous cancelled login still holding it).
    pub(crate) fn bind(port: u16, provider_label: &str) -> Result<Self> {
        let v4 = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .map_err(|error| bind_error(&error, port, provider_label))?;
        let mut listeners = vec![v4];
        // IPv6 loopback is best-effort -- a host without IPv6 must not fail the
        // login -- with one exception: an `AddrInUse` on `[::1]:port` while the
        // IPv4 bind succeeded means another local process already holds the IPv6
        // callback. Because `localhost` can resolve to `::1`, the browser could
        // deliver the authorization code (and Anthropic's state/verifier) to that
        // process, so refuse rather than risk leaking the code.
        match TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port))) {
            Ok(v6) => listeners.push(v6),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                return Err(bind_error(&error, port, provider_label));
            }
            Err(_) => {}
        }
        for listener in &listeners {
            listener
                .set_nonblocking(true)
                .context("failed to configure OAuth callback listener")?;
        }
        Ok(Self { listeners, port })
    }

    /// Wait for a valid authorization code, returning promptly on cancellation,
    /// the overall deadline, a terminal OAuth error, or a pasted manual code.
    ///
    /// Wrong-path requests get a 404 and the loop keeps waiting; state-mismatch
    /// requests get a 400 and the loop keeps waiting (a bad-state code is never
    /// accepted); a provider `error=` callback gets a 400 and ends the flow.
    pub(crate) fn wait_for_code(
        &self,
        config: &CallbackConfig,
        deadline: Instant,
        cancel: &CancellationToken,
        manual_rx: Option<&Receiver<String>>,
    ) -> Result<String> {
        loop {
            if cancel.is_cancelled() {
                bail!("{} login cancelled", config.provider_label);
            }
            if Instant::now() >= deadline {
                bail!(
                    "{} login timed out waiting for the browser callback on port {}; retry the login, or paste the authorization code or full redirect URL",
                    config.provider_label,
                    self.port
                );
            }
            // A pasted code/redirect short-circuits the browser wait.
            if let Some(rx) = manual_rx
                && let Ok(input) = rx.try_recv()
                && !input.trim().is_empty()
            {
                return parse_manual_authorization(&input, config.expected_state);
            }
            if let Some(code) = self.try_accept(config, cancel)? {
                return Ok(code);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Drain any pending connection across the bound listeners. Returns
    /// `Ok(Some(code))` on a valid code, `Ok(None)` when nothing actionable is
    /// pending, and `Err` only on a terminal OAuth error callback.
    fn try_accept(
        &self,
        config: &CallbackConfig,
        cancel: &CancellationToken,
    ) -> Result<Option<String>> {
        for listener in &self.listeners {
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // A malformed / timed-out / cancelled read closes that
                        // connection and keeps the loop alive instead of failing
                        // the login.
                        let Ok(request) = read_http_request(&mut stream, cancel) else {
                            continue;
                        };
                        match parse_callback_target(
                            &request,
                            config.expected_state,
                            config.callback_path,
                        ) {
                            CallbackOutcome::Code(code) => {
                                let _ = write_callback_response(
                                    &mut stream,
                                    200,
                                    config.success_message,
                                );
                                return Ok(Some(code));
                            }
                            CallbackOutcome::OAuthError(error) => {
                                let _ = write_callback_response(
                                    &mut stream,
                                    400,
                                    "Authentication failed. You can close this window.",
                                );
                                bail!("{} authorization failed: {error}", config.provider_label);
                            }
                            CallbackOutcome::StateMismatch => {
                                let _ = write_callback_response(
                                    &mut stream,
                                    400,
                                    "State mismatch. You can close this window.",
                                );
                            }
                            CallbackOutcome::WrongPath | CallbackOutcome::Missing => {
                                let _ = write_callback_response(&mut stream, 404, "Not found.");
                            }
                        }
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    // A transient accept failure (e.g. ECONNABORTED, or EMFILE
                    // under temporary descriptor pressure) must not fail the
                    // whole login: log it so a persistent failure is observable,
                    // stop draining this listener, and let the outer poll loop
                    // retry until the overall deadline.
                    Err(error) => {
                        tracing::warn!(
                            "transient error accepting OAuth callback (will keep waiting): {error}"
                        );
                        break;
                    }
                }
            }
        }
        Ok(None)
    }
}

fn bind_error(error: &std::io::Error, port: u16, provider_label: &str) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::AddrInUse {
        anyhow!(
            "failed to start {provider_label} OAuth callback server: port {port} is already in use (a previous cancelled login may still hold it; wait a few seconds and retry)"
        )
    } else {
        anyhow!("failed to start {provider_label} OAuth callback server on port {port}: {error}")
    }
}

/// Read one callback HTTP request head with a non-blocking, cancellable loop.
/// The accepted stream is put in non-blocking mode and polled on `POLL_INTERVAL`
/// so a TUI cancel releases the port within a poll tick even when a client
/// connects but stalls without sending the request line, and a missing/failed
/// socket timeout cannot wedge the thread or busy-spin the CPU. The total read
/// is bounded by `READ_TIMEOUT` and a 16 KiB cap so a slow or oversized client
/// cannot wedge or grow it.
fn read_http_request(stream: &mut TcpStream, cancel: &CancellationToken) -> Result<String> {
    stream
        .set_nonblocking(true)
        .context("failed to configure OAuth callback connection")?;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        if cancel.is_cancelled() {
            bail!("OAuth callback read cancelled");
        }
        if Instant::now() >= deadline {
            break;
        }
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(len) => {
                request.extend_from_slice(&buffer[..len]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                if request.len() > 16 * 1024 {
                    break;
                }
            }
            // No data yet: sleep one poll tick (bounding CPU) and re-check the
            // cancellation token and deadline.
            Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(anyhow!("failed to read OAuth callback: {error}")),
        }
    }
    Ok(String::from_utf8_lossy(&request).into_owned())
}

fn write_callback_response(stream: &mut TcpStream, status: u16, body: &str) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .context("failed to write OAuth callback response")
}

/// Classify one HTTP request line against the expected callback path + state.
pub(crate) fn parse_callback_target(
    request: &str,
    expected_state: &str,
    callback_path: &str,
) -> CallbackOutcome {
    let Some(target) = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
    else {
        return CallbackOutcome::Missing;
    };
    let parsed = if target.starts_with('/') {
        Url::parse(&format!("http://localhost{target}"))
    } else {
        Url::parse(target)
    };
    let Ok(url) = parsed else {
        return CallbackOutcome::Missing;
    };
    if url.path() != callback_path {
        return CallbackOutcome::WrongPath;
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error.filter(|error| !error.is_empty()) {
        return CallbackOutcome::OAuthError(error);
    }
    match code.filter(|code| !code.is_empty()) {
        Some(code) if state.as_deref() == Some(expected_state) => CallbackOutcome::Code(code),
        Some(_) => CallbackOutcome::StateMismatch,
        None => CallbackOutcome::Missing,
    }
}

/// Parse a manually pasted authorization input: a full redirect URL, a bare
/// query string containing `code=...`, a `code#state` pair, or a bare code.
/// A present-but-mismatched `state` is rejected.
pub(crate) fn parse_manual_authorization(input: &str, expected_state: &str) -> Result<String> {
    let value = input.trim();
    if value.is_empty() {
        bail!("no authorization code was provided");
    }
    let parsed = extract_code_state(value);
    // A pasted terminal error redirect (e.g. `?error=access_denied`) surfaces the
    // provider failure rather than the generic "no code found" message.
    if let Some(error) = parsed.error.filter(|error| !error.is_empty()) {
        bail!("authorization failed: {error}");
    }
    let code = parsed
        .code
        .map(|code| code.trim().to_string())
        .filter(|code| !code.is_empty())
        .ok_or_else(|| anyhow!("could not find an authorization code in the pasted input"))?;
    if let Some(state) = parsed.state.filter(|state| !state.is_empty())
        && state != expected_state
    {
        bail!("OAuth state mismatch");
    }
    Ok(code)
}

/// Components pulled from a pasted authorization input.
struct ManualAuthorization {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Pull `code`/`state`/`error` from a pasted value. Anthropic returns the code as
/// `code#state`, so a `#` in the code is split out into the state when no
/// explicit state was found.
fn extract_code_state(value: &str) -> ManualAuthorization {
    // A full http(s) redirect URL: take code/state/error from the query, and
    // treat a URL fragment as state if the query carried none.
    if let Ok(url) = Url::parse(value)
        && (url.scheme() == "http" || url.scheme() == "https")
    {
        let (code, mut state, error) = collect_pairs(url.query_pairs());
        if state.is_none()
            && let Some(fragment) = url.fragment().filter(|fragment| !fragment.is_empty())
        {
            state = Some(fragment.to_string());
        }
        let (code, state) = normalize_code_state(code, state);
        return ManualAuthorization { code, state, error };
    }
    // A path-prefixed (`callback?code=...`), leading-`?`/`#`, or bare query
    // string carrying `code=` or `error=`. Parse only the query/fragment portion
    // after the first `?`/`#` so a path prefix is not folded into the first key.
    if value.contains("code=") || value.contains("error=") {
        let query = match value.find(['?', '#']) {
            Some(index) => &value[index + 1..],
            None => value,
        };
        if let Ok(url) = Url::parse(&format!("http://localhost/?{query}")) {
            let (code, mut state, error) = collect_pairs(url.query_pairs());
            // Consistent with the full-URL branch: a `#fragment` carries the
            // state when the query had none (e.g. `callback?code=abc#state`).
            if state.is_none()
                && let Some(fragment) = url.fragment().filter(|fragment| !fragment.is_empty())
            {
                state = Some(fragment.to_string());
            }
            let (code, state) = normalize_code_state(code, state);
            return ManualAuthorization { code, state, error };
        }
    }
    // A `code#state` pair, or a bare code.
    let (code, state) = normalize_code_state(Some(value.to_string()), None);
    ManualAuthorization {
        code,
        state,
        error: None,
    }
}

/// Collect `(code, state, error)` from a set of query pairs.
fn collect_pairs<'a>(
    pairs: impl Iterator<Item = (std::borrow::Cow<'a, str>, std::borrow::Cow<'a, str>)>,
) -> (Option<String>, Option<String>, Option<String>) {
    let (mut code, mut state, mut error) = (None, None, None);
    for (key, item) in pairs {
        match key.as_ref() {
            "code" => code = Some(item.into_owned()),
            "state" => state = Some(item.into_owned()),
            "error" => error = Some(item.into_owned()),
            _ => {}
        }
    }
    (code, state, error)
}

/// Split a trailing `#state` out of the code when the state is otherwise unknown.
fn normalize_code_state(
    code: Option<String>,
    state: Option<String>,
) -> (Option<String>, Option<String>) {
    match (code, state) {
        (Some(code), None) => match code.split_once('#') {
            Some((code, state)) => (Some(code.to_string()), Some(state.to_string())),
            None => (Some(code), None),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const PATH: &str = "/callback";

    #[test]
    fn pkce_challenge_is_base64url_sha256_of_verifier() {
        let pkce = create_pkce();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
        assert!(!pkce.verifier.is_empty());
    }

    #[test]
    fn random_url_tokens_differ() {
        assert_ne!(random_url_token(32), random_url_token(32));
    }

    #[test]
    fn parses_relative_and_absolute_callback_code() {
        let relative = "GET /callback?code=abc123&state=st HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(
            parse_callback_target(relative, "st", PATH),
            CallbackOutcome::Code("abc123".to_string())
        );
        let absolute = "GET http://localhost:53692/callback?code=abc&state=st HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(
            parse_callback_target(absolute, "st", PATH),
            CallbackOutcome::Code("abc".to_string())
        );
    }

    #[test]
    fn state_mismatch_is_classified_not_accepted() {
        let request = "GET /callback?code=abc&state=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(
            parse_callback_target(request, "st", PATH),
            CallbackOutcome::StateMismatch
        );
    }

    #[test]
    fn wrong_path_and_missing_code_are_distinguished() {
        let favicon = "GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(
            parse_callback_target(favicon, "st", PATH),
            CallbackOutcome::WrongPath
        );
        let no_code = "GET /callback?state=st HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(
            parse_callback_target(no_code, "st", PATH),
            CallbackOutcome::Missing
        );
    }

    #[test]
    fn oauth_error_callback_is_terminal() {
        let request =
            "GET /callback?error=access_denied&state=st HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(
            parse_callback_target(request, "st", PATH),
            CallbackOutcome::OAuthError("access_denied".to_string())
        );
    }

    #[test]
    fn manual_input_accepts_url_query_and_bare_code() {
        assert_eq!(
            parse_manual_authorization("http://localhost:53692/callback?code=abc&state=st", "st")
                .unwrap(),
            "abc"
        );
        assert_eq!(
            parse_manual_authorization("code=abc&state=st", "st").unwrap(),
            "abc"
        );
        assert_eq!(parse_manual_authorization("abc#st", "st").unwrap(), "abc");
        assert_eq!(
            parse_manual_authorization("bare-code", "st").unwrap(),
            "bare-code"
        );
    }

    #[test]
    fn manual_input_rejects_state_mismatch() {
        let error = parse_manual_authorization("code=abc&state=wrong", "st")
            .unwrap_err()
            .to_string();
        assert!(error.contains("state mismatch"), "got: {error}");
        let error = parse_manual_authorization("abc#wrong", "st")
            .unwrap_err()
            .to_string();
        assert!(error.contains("state mismatch"), "got: {error}");
    }

    #[test]
    fn manual_input_rejects_empty() {
        assert!(parse_manual_authorization("   ", "st").is_err());
    }

    #[test]
    fn manual_input_handles_path_prefixed_query() {
        // A pasted value with a path before the query must not fold the path into
        // the first query key.
        assert_eq!(
            parse_manual_authorization("callback?code=abc&state=st", "st").unwrap(),
            "abc"
        );
        assert_eq!(
            parse_manual_authorization("/callback?code=abc&state=st", "st").unwrap(),
            "abc"
        );
        // A `#fragment` after a path-prefixed query carries the state, matching
        // the full-URL branch.
        assert_eq!(
            parse_manual_authorization("callback?code=abc#st", "st").unwrap(),
            "abc"
        );
        assert!(
            parse_manual_authorization("callback?code=abc#wrong", "st").is_err(),
            "fragment state is validated"
        );
    }

    #[test]
    fn manual_input_surfaces_oauth_error_redirect() {
        let error = parse_manual_authorization(
            "http://localhost:53692/callback?error=access_denied&state=st",
            "st",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("access_denied"), "got: {error}");
        // The bare-query form surfaces the error too.
        let error = parse_manual_authorization("error=access_denied", "st")
            .unwrap_err()
            .to_string();
        assert!(error.contains("access_denied"), "got: {error}");
    }

    #[test]
    fn wait_for_code_returns_pasted_manual_code() {
        let server = CallbackServer::bind(0, "test").expect("bind ephemeral");
        // bind(0) actually binds an OS-chosen port; that is fine for the manual
        // path which never touches the socket.
        let (tx, rx) = mpsc::channel::<String>();
        tx.send("abc#st".to_string()).unwrap();
        let cancel = CancellationToken::new();
        let config = CallbackConfig {
            expected_state: "st",
            callback_path: PATH,
            success_message: "ok",
            provider_label: "test",
        };
        let code = server
            .wait_for_code(&config, Instant::now() + LOGIN_TIMEOUT, &cancel, Some(&rx))
            .expect("manual code");
        assert_eq!(code, "abc");
    }

    #[test]
    fn wait_for_code_returns_promptly_on_cancel() {
        let server = CallbackServer::bind(0, "test").expect("bind ephemeral");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let config = CallbackConfig {
            expected_state: "st",
            callback_path: PATH,
            success_message: "ok",
            provider_label: "test",
        };
        let started = Instant::now();
        let error = server
            .wait_for_code(&config, Instant::now() + LOGIN_TIMEOUT, &cancel, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cancelled"), "got: {error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn ipv6_addr_in_use_is_fatal_to_avoid_callback_hijack() {
        // Hold an ephemeral port on IPv4; bind the matching [::1] first so a
        // second bind on that port fails with AddrInUse. If IPv6 loopback is
        // unavailable on this host the scenario cannot be built, so the test is a
        // no-op rather than a false failure.
        let Ok(v6_holder) = TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, 0))) else {
            return;
        };
        let port = v6_holder.local_addr().unwrap().port();
        // IPv4 on the same port must be free for the v4 bind to succeed; if it is
        // taken, skip (cannot construct the targeted v4-ok/v6-in-use case).
        if TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).is_err() {
            return;
        }
        let error = CallbackServer::bind(port, "test").unwrap_err().to_string();
        assert!(error.contains("already in use"), "got: {error}");
    }

    #[test]
    fn wait_for_code_times_out_with_actionable_message() {
        let server = CallbackServer::bind(0, "test").expect("bind ephemeral");
        let cancel = CancellationToken::new();
        let config = CallbackConfig {
            expected_state: "st",
            callback_path: PATH,
            success_message: "ok",
            provider_label: "test",
        };
        // A deadline already in the past times out on the first iteration.
        let error = server
            .wait_for_code(&config, Instant::now(), &cancel, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"), "got: {error}");
        assert!(error.contains("paste"), "actionable hint: {error}");
    }
}
