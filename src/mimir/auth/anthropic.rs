//! Anthropic "Claude Code subscription" auth: reads the Claude Code OAuth token
//! (from the Iris auth store, or bootstrapped from Claude Code's own credential
//! file -- and, on macOS only, the login Keychain when that file is unreadable),
//! refreshes it near expiry, and persists the rotated token back to the same
//! source so a stale refresh token never locks the user out of Claude Code.
//!
//! The credential-safety rules mirror minimalcc-pi's `src/credentials.ts`:
//! source-keyed refresh coalescing (concurrent refreshes do not all hit the
//! token endpoint), best-effort stale-write avoidance (a newer fresh token from
//! another process is preferred over our own exchange result), scope
//! preservation with a sane default, and fully redacted errors (no token,
//! refresh token, credential JSON, Keychain output, or response body ever
//! appears in a surfaced error).
//!
//! ponytail: only the Claude Code subscription OAuth lane (no x-api-key, no
//! login flow here, no thinking replay). Login is owned elsewhere; this module
//! only loads, refreshes, and writes back the token from whichever source held
//! it.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use serde_json::{Map, Value, json};

use crate::mimir::auth::storage::{AuthStore, OAuthCredentials};

/// Auth-store provider key for Claude Code subscription OAuth credentials.
pub(crate) const AUTH_PROVIDER: &str = "anthropic";

const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Scopes requested when the stored credential records none. Mirrors the scopes
/// Claude Code itself asks for.
const DEFAULT_SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
/// macOS login-Keychain service that Claude Code stores its credential under.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
/// Refresh this far ahead of expiry so an in-flight request never races the
/// token going stale.
const REFRESH_MARGIN_MS: u128 = 300_000;
/// Sentinel expiry for a credential that records no `expiresAt`: it is treated
/// as "never near expiry", so it is used as-is and never auto-refreshed (a
/// rejected one still heals through the 401 force-refresh path).
const NO_EXPIRY: u128 = u128::MAX;

/// Where a loaded token came from, so a refreshed token is written back to the
/// same place (data-integrity critical: the refresh token rotates).
#[derive(Debug, Clone)]
enum CredentialSource {
    IrisStore,
    ClaudeCodeFile(PathBuf),
    /// Loaded from the macOS Keychain because the credential file was
    /// unreadable. A refresh is persisted to the standard credential-file path
    /// (the `PathBuf`) so subsequent requests read it from disk.
    ClaudeCodeKeychain(PathBuf),
}

/// Parsed Claude Code OAuth token-refresh response (no expiry math; that is
/// applied against the injected clock by [`build_refreshed`]).
struct RefreshResponse {
    access: String,
    refresh: Option<String>,
    expires_in_secs: u64,
    scope: Option<String>,
}

/// Injectable seams so the load/refresh logic is exercised with a fake clock,
/// fake Keychain runner, and fake token exchange -- never real network,
/// Keychain, or user credentials. Production wires the real implementations.
struct Seams<'a> {
    /// Target OS string (`std::env::consts::OS`): gates the Keychain fallback.
    platform: &'a str,
    now_ms: u128,
    run_security: &'a dyn Fn(&[&str]) -> Result<String>,
    exchange:
        &'a dyn Fn(&str /* refresh_token */, &str /* scope */) -> Result<RefreshResponse>,
}

#[derive(Debug, Clone)]
pub(crate) struct AnthropicTokenStore {
    storage: AuthStore,
}

impl AnthropicTokenStore {
    pub(crate) fn from_env() -> Result<Self> {
        Ok(Self {
            storage: AuthStore::from_env()?,
        })
    }

    /// Return the current OAuth bearer access token, refreshing (and persisting)
    /// it first when it is at/near expiry.
    pub(crate) fn access_token(&self, client: &Client) -> Result<String> {
        self.resolve(client, false, None)
    }

    /// Force a token refresh regardless of cached expiry (used after an HTTP
    /// 401/403), persist it back to its source, and return the new bearer token.
    /// `previous` is the rejected token: a coalesced refresh only short-circuits
    /// on a token that actually differs from it, so the rejected one is never
    /// handed back.
    pub(crate) fn force_refresh(&self, client: &Client, previous: Option<&str>) -> Result<String> {
        self.resolve(client, true, previous)
    }

    /// Wire the production seams (real clock, real `/usr/bin/security`, real
    /// token endpoint) and resolve a bearer token.
    fn resolve(&self, client: &Client, force: bool, previous: Option<&str>) -> Result<String> {
        let path = claude_code_credentials_path()?;
        let run_security = |args: &[&str]| run_macos_security(args);
        let exchange =
            |refresh_token: &str, scope: &str| exchange_refresh_token(client, refresh_token, scope);
        let seams = Seams {
            platform: env::consts::OS,
            now_ms: now_millis(),
            run_security: &run_security,
            exchange: &exchange,
        };
        self.resolve_with(force, previous, &path, &seams)
    }

    /// Seam-driven core: load from the preferred source, return the cached token
    /// when it is fresh and no force is requested, otherwise refresh under a
    /// per-source lock.
    fn resolve_with(
        &self,
        force: bool,
        previous: Option<&str>,
        path: &Path,
        seams: &Seams,
    ) -> Result<String> {
        let (loaded, source) = self.load(path, seams)?;
        if !force && !is_near_expiry(loaded.expires, seams.now_ms) {
            return Ok(loaded.access);
        }
        // Coalesce concurrent refreshes that persist to the same destination:
        // whichever thread refreshes first writes the token; the rest re-read it
        // under the lock instead of starting another token exchange.
        let lock = refresh_lock_for(&self.source_key(&source));
        let _guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
        self.refresh_locked(force, previous, &source, &loaded, seams)
    }

    /// Load credentials, preferring the Iris store, then the Claude Code
    /// credential file, then (macOS only, file unreadable) the login Keychain.
    fn load(&self, path: &Path, seams: &Seams) -> Result<(OAuthCredentials, CredentialSource)> {
        if let Ok(credentials) = self.storage.oauth_credentials(AUTH_PROVIDER) {
            return Ok((credentials, CredentialSource::IrisStore));
        }
        match fs::read_to_string(path) {
            Ok(raw) => {
                let credentials = parse_credentials_json(&raw, path)?;
                Ok((
                    credentials,
                    CredentialSource::ClaudeCodeFile(path.to_path_buf()),
                ))
            }
            Err(_) if seams.platform == "macos" => {
                let credentials = load_from_keychain(path, seams.run_security)?;
                Ok((
                    credentials,
                    CredentialSource::ClaudeCodeKeychain(path.to_path_buf()),
                ))
            }
            Err(_) => Err(credential_error(
                path,
                "Claude Code credentials could not be read",
            )),
        }
    }

    /// Refresh under the per-source lock. Re-reads the persistence target first
    /// (coalescing) and again after the exchange (stale-write avoidance).
    fn refresh_locked(
        &self,
        force: bool,
        previous: Option<&str>,
        source: &CredentialSource,
        loaded: &OAuthCredentials,
        seams: &Seams,
    ) -> Result<String> {
        let current = self.reread(source);
        let base = current.as_ref().unwrap_or(loaded);

        // Another refresh already landed: reuse its token instead of exchanging.
        if force {
            if let Some(prev) = previous
                && let Some(token) = fresh_changed(base, prev, seams.now_ms)
            {
                return Ok(token);
            }
        } else if let Some(token) = fresh_unexpired(base, seams.now_ms) {
            return Ok(token);
        }

        let token_before = base.access.clone();
        if base.refresh.trim().is_empty() {
            return Err(credential_error(
                &self.source_path(source),
                "Claude Code OAuth access token is expired and no refresh token is available",
            ));
        }
        // Wrap any exchange failure in a redacted login-hint so every refresh
        // error is actionable (the exchange seam never carries secret material).
        let response = (seams.exchange)(&base.refresh, &scopes_for(base))
            .map_err(|error| credential_error(&self.source_path(source), &error.to_string()))?;
        let refreshed = build_refreshed(base, &response, seams.now_ms);

        // Persist only if nobody else wrote a newer fresh token meanwhile. A
        // forced refresh additionally refuses the rejected `previous` token, so
        // an external writer that reintroduced it cannot make us hand it back.
        if let Some(token) = self.reread(source).and_then(|concurrent| {
            let token = fresh_changed(&concurrent, &token_before, seams.now_ms)?;
            match previous {
                Some(prev) if token == prev => None,
                _ => Some(token),
            }
        }) {
            return Ok(token);
        }
        self.persist(source, &refreshed)?;
        Ok(refreshed.access)
    }

    /// Re-read the persistence target for a source (the auth store, or the
    /// credential file -- Keychain refreshes are written to that file). Failure
    /// is "no current credential", not an error: callers fall back to `loaded`.
    fn reread(&self, source: &CredentialSource) -> Option<OAuthCredentials> {
        match source {
            CredentialSource::IrisStore => self.storage.oauth_credentials(AUTH_PROVIDER).ok(),
            CredentialSource::ClaudeCodeFile(path) | CredentialSource::ClaudeCodeKeychain(path) => {
                let raw = fs::read_to_string(path).ok()?;
                parse_credentials_json(&raw, path).ok()
            }
        }
    }

    fn persist(&self, source: &CredentialSource, credentials: &OAuthCredentials) -> Result<()> {
        match source {
            CredentialSource::IrisStore => self
                .storage
                .set_oauth_credentials(AUTH_PROVIDER, credentials.clone()),
            CredentialSource::ClaudeCodeFile(path) | CredentialSource::ClaudeCodeKeychain(path) => {
                write_claude_code_file(path, credentials)
            }
        }
    }

    /// Lock key for refresh coalescing: the persistence destination, so file and
    /// Keychain sources that write the same file share one lock.
    fn source_key(&self, source: &CredentialSource) -> String {
        match source {
            CredentialSource::IrisStore => {
                format!("iris-store:{}", self.storage.path().display())
            }
            CredentialSource::ClaudeCodeFile(path) | CredentialSource::ClaudeCodeKeychain(path) => {
                format!("claude-file:{}", path.display())
            }
        }
    }

    /// Path used only for the login-hint in errors (never a secret).
    fn source_path(&self, source: &CredentialSource) -> PathBuf {
        match source {
            CredentialSource::IrisStore => self.storage.path().to_path_buf(),
            CredentialSource::ClaudeCodeFile(path) | CredentialSource::ClaudeCodeKeychain(path) => {
                path.clone()
            }
        }
    }
}

/// Whether a Claude Code credential file exists to bootstrap from. Used by the
/// model catalog to mark Anthropic available even when Iris's own auth store has
/// no stored credential. Only checks for the file's presence -- it never reads,
/// parses, or exposes the secret, and (ponytail) does not probe the macOS
/// Keychain, which would exec `/usr/bin/security` and could trigger an unlock
/// prompt on every status render.
pub(crate) fn claude_code_credentials_available() -> bool {
    claude_code_credentials_path()
        .map(|path| path.exists())
        .unwrap_or(false)
}

fn claude_code_credentials_path() -> Result<PathBuf> {
    if let Ok(dir) = env::var("CLAUDE_CONFIG_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Ok(Path::new(dir).join(".credentials.json"));
        }
    }
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(Path::new(&home).join(".claude/.credentials.json"))
}

/// A login-hint error with no secret material. Every credential load/refresh
/// failure flows through here so a token, refresh token, credential blob,
/// Keychain output, or response body can never reach the surfaced message.
fn credential_error(path: &Path, reason: &str) -> anyhow::Error {
    anyhow!(
        "{reason}. Run Claude Code login, then ensure Claude Code credentials exist at {}",
        path.display()
    )
}

/// Parse credential JSON, tolerating both the nested `{"claudeAiOauth":{...}}`
/// shape and a flat object. Errors are redacted (never echo the raw JSON).
fn parse_credentials_json(raw: &str, path: &Path) -> Result<OAuthCredentials> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| credential_error(path, "Claude Code credentials are malformed"))?;
    parse_claude_code_credentials(&value, path)
}

fn parse_claude_code_credentials(value: &Value, path: &Path) -> Result<OAuthCredentials> {
    let oauth = value.get("claudeAiOauth").unwrap_or(value);
    let access = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| credential_error(path, "Claude Code credentials are missing accessToken"))?;
    // refreshToken and expiresAt are optional: a flat `{accessToken}` is a valid
    // (older) shape. A missing refresh token only fails if a refresh is needed;
    // a missing expiry means "never near expiry".
    let refresh = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let expires = oauth
        .get("expiresAt")
        .and_then(Value::as_u64)
        .map(u128::from)
        .unwrap_or(NO_EXPIRY);
    let mut extra = Map::new();
    if let Some(scopes) = oauth.get("scopes") {
        extra.insert("scopes".to_string(), scopes.clone());
    }
    Ok(OAuthCredentials {
        access: access.to_string(),
        refresh,
        expires,
        extra,
    })
}

/// macOS-only: read the Claude Code credential from the login Keychain. Both a
/// `security` failure and a malformed blob redact to a login hint.
fn load_from_keychain(
    path: &Path,
    run_security: &dyn Fn(&[&str]) -> Result<String>,
) -> Result<OAuthCredentials> {
    let raw =
        run_security(&["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"]).map_err(|_| {
            credential_error(
                path,
                "Claude Code credentials could not be read from the macOS Keychain",
            )
        })?;
    parse_credentials_json(&raw, path)
}

/// Space-joined stored scopes, falling back to the Claude Code defaults when the
/// credential records none (or only blanks).
fn scopes_for(credentials: &OAuthCredentials) -> String {
    match credentials.extra.get("scopes") {
        Some(Value::Array(items)) => {
            let scopes: Vec<&str> = items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .collect();
            if scopes.is_empty() {
                DEFAULT_SCOPES.to_string()
            } else {
                scopes.join(" ")
            }
        }
        Some(Value::String(scopes)) if !scopes.trim().is_empty() => scopes.trim().to_string(),
        _ => DEFAULT_SCOPES.to_string(),
    }
}

fn is_near_expiry(expires: u128, now_ms: u128) -> bool {
    expires <= now_ms.saturating_add(REFRESH_MARGIN_MS)
}

/// The access token if it is non-empty and not near expiry, else `None`.
fn fresh_unexpired(credentials: &OAuthCredentials, now_ms: u128) -> Option<String> {
    (!credentials.access.trim().is_empty() && !is_near_expiry(credentials.expires, now_ms))
        .then(|| credentials.access.clone())
}

/// The access token if it is fresh AND differs from `previous` -- i.e. a token
/// another refresh produced, not the (possibly rejected) one we already hold.
fn fresh_changed(credentials: &OAuthCredentials, previous: &str, now_ms: u128) -> Option<String> {
    fresh_unexpired(credentials, now_ms).filter(|token| token != previous)
}

/// Build the rotated credential: keep the prior refresh token / scopes when the
/// response omits them, and stamp expiry against the injected clock.
fn build_refreshed(
    old: &OAuthCredentials,
    response: &RefreshResponse,
    now_ms: u128,
) -> OAuthCredentials {
    let mut extra = old.extra.clone();
    if let Some(scope) = &response.scope {
        let scopes: Vec<Value> = scope
            .split_whitespace()
            .map(|scope| Value::String(scope.to_string()))
            .collect();
        if !scopes.is_empty() {
            extra.insert("scopes".to_string(), Value::Array(scopes));
        }
    }
    OAuthCredentials {
        access: response.access.clone(),
        refresh: response
            .refresh
            .clone()
            .unwrap_or_else(|| old.refresh.clone()),
        expires: now_ms + u128::from(response.expires_in_secs) * 1000,
        extra,
    }
}

/// Write the rotated token back into the Claude Code file, updating only the
/// credential fields IN PLACE so every other key the user has (nested
/// `claudeAiOauth` siblings like subscriptionType, or unrelated root keys) is
/// preserved, and the file's existing shape (nested vs flat) is kept. Atomic
/// (tmp + rename) and 0600 -- a stale refresh token here would lock the user out
/// of Claude Code, so this must never drop or reshape their config.
fn write_claude_code_file(path: &Path, credentials: &OAuthCredentials) -> Result<()> {
    // Preserve the whole existing document; default to a nested envelope only
    // when the file is absent or not a JSON object.
    let mut document = match fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    {
        Some(value) if value.is_object() => value,
        _ => json!({ "claudeAiOauth": {} }),
    };
    let root = document
        .as_object_mut()
        .expect("document is a JSON object by construction");
    // Update the credential fields wherever they live: under `claudeAiOauth`
    // when that envelope exists, otherwise at the (flat) root.
    let target = if root.contains_key("claudeAiOauth") {
        root.get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| anyhow!("Claude Code claudeAiOauth field is not an object"))?
    } else {
        root
    };
    target.insert("accessToken".to_string(), json!(credentials.access));
    target.insert("refreshToken".to_string(), json!(credentials.refresh));
    target.insert("expiresAt".to_string(), json!(credentials.expires as u64));
    // Only write scopes when the refresh observed them, otherwise leave the
    // file's existing scopes untouched (in-place preservation above).
    if let Some(scopes) = credentials.extra.get("scopes") {
        target.insert("scopes".to_string(), scopes.clone());
    }
    let raw = serde_json::to_string_pretty(&document)?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp_path = unique_tmp_path(path);
    write_secret_file(&tmp_path, &format!("{raw}\n"))?;
    fs::rename(&tmp_path, path).with_context(|| format!("failed to replace {}", path.display()))
}

fn unique_tmp_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "tmp-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ))
}

/// Create (or truncate) a file containing secret material and write `contents`.
/// On Unix the file is created with 0600 from the start, closing the TOCTOU
/// window where a default-umask (0644) temp file briefly exposes the token to
/// other local users before a later chmod.
fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Run the macOS `security` CLI, returning only stdout. Errors are deliberately
/// opaque: the raw tool output may name the Keychain item and must not surface.
fn run_macos_security(args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/security")
        .args(args)
        .output()
        .map_err(|_| anyhow!("failed to run security"))?;
    if !output.status.success() {
        return Err(anyhow!("security command failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Production token exchange against the Claude Code OAuth endpoint. The
/// response body is the highest-risk surface, so it is never included in errors.
fn exchange_refresh_token(
    client: &Client,
    refresh_token: &str,
    scope: &str,
) -> Result<RefreshResponse> {
    let response = client
        .post(TOKEN_URL)
        .header("anthropic-beta", OAUTH_BETA)
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
            "scope": scope,
        }))
        .send()
        .map_err(|_| anyhow!("Claude Code OAuth token refresh failed"))?;

    let status = response.status();
    if !status.is_success() {
        let _ = response.text();
        return Err(anyhow!(
            "Claude Code OAuth token refresh failed with HTTP {}",
            status.as_u16()
        ));
    }
    let body: Value = response
        .json()
        .map_err(|_| anyhow!("Claude Code OAuth token refresh response is malformed"))?;
    parse_refresh_response(&body)
}

/// Parse the token-refresh response. Field-shape errors name only the missing
/// fields, never their values. Pure so it is unit-tested without network.
fn parse_refresh_response(body: &Value) -> Result<RefreshResponse> {
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty());
    let expires_in = body.get("expires_in").and_then(Value::as_u64);
    let (Some(access), Some(expires_in_secs)) = (access, expires_in) else {
        return Err(anyhow!(
            "Claude Code OAuth token refresh response is missing required fields"
        ));
    };
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    let scope = body
        .get("scope")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_string);
    Ok(RefreshResponse {
        access: access.to_string(),
        refresh,
        expires_in_secs,
        scope,
    })
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

/// Per-source refresh lock registry. Concurrent refreshes that persist to the
/// same destination serialize on one lock; the loser re-reads the freshly
/// written token instead of starting a second token exchange.
///
/// ponytail: process-global map keyed by persistence path; fine for the handful
/// of credential sources Iris uses. Swap to a sharded map only if lock-map
/// contention ever shows up.
fn refresh_lock_for(key: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let registry = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = registry.lock().unwrap_or_else(|poison| poison.into_inner());
    Arc::clone(
        guard
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const NOW: u128 = 1_700_000_000_000;
    const HOUR_MS: u128 = 60 * 60 * 1000;

    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!(
            "iris-cc-cred-{tag}-{nanos}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A token store whose Iris auth file does not exist, so loads fall through
    /// to the Claude Code credential file / Keychain.
    fn store_without_iris_creds(dir: &Path) -> AnthropicTokenStore {
        AnthropicTokenStore {
            storage: AuthStore::from_path(dir.join("auth.json")),
        }
    }

    fn nested_credentials(fields: Value) -> Value {
        let mut oauth = json!({
            "accessToken": "fake-access-token",
            "refreshToken": "fake-refresh-token",
            "expiresAt": NOW + HOUR_MS,
            "scopes": ["user:profile", "user:inference"],
            "subscriptionType": "max",
        });
        let oauth_map = oauth.as_object_mut().unwrap();
        for (key, value) in fields.as_object().unwrap() {
            oauth_map.insert(key.clone(), value.clone());
        }
        json!({ "claudeAiOauth": oauth })
    }

    fn write_credentials(path: &Path, content: &Value) {
        fs::write(path, serde_json::to_string_pretty(content).unwrap()).unwrap();
    }

    fn read_oauth(path: &Path) -> Value {
        let value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        value["claudeAiOauth"].clone()
    }

    fn seams<'a>(
        platform: &'a str,
        run_security: &'a dyn Fn(&[&str]) -> Result<String>,
        exchange: &'a dyn Fn(&str, &str) -> Result<RefreshResponse>,
    ) -> Seams<'a> {
        Seams {
            platform,
            now_ms: NOW,
            run_security,
            exchange,
        }
    }

    fn never_security(_: &[&str]) -> Result<String> {
        Err(anyhow!("security must not run"))
    }

    fn never_exchange(_: &str, _: &str) -> Result<RefreshResponse> {
        Err(anyhow!("exchange must not run"))
    }

    // ---- pure helpers --------------------------------------------------------

    #[test]
    fn parses_nested_and_flat_credentials() -> Result<()> {
        let nested = parse_claude_code_credentials(
            &json!({ "claudeAiOauth": { "accessToken": "a", "refreshToken": "r", "expiresAt": 7_u64 } }),
            Path::new("/x"),
        )?;
        assert_eq!(nested.access, "a");
        assert_eq!(nested.refresh, "r");
        assert_eq!(nested.expires, 7);

        // Flat top-level token with no refresh/expiry is accepted (older shape).
        let flat =
            parse_claude_code_credentials(&json!({ "accessToken": "flat" }), Path::new("/x"))?;
        assert_eq!(flat.access, "flat");
        assert_eq!(flat.refresh, "");
        assert_eq!(flat.expires, NO_EXPIRY);
        Ok(())
    }

    #[test]
    fn scopes_default_when_absent_or_blank() {
        let with_scopes = parse_claude_code_credentials(
            &json!({ "accessToken": "a", "scopes": ["user:inference", " "] }),
            Path::new("/x"),
        )
        .unwrap();
        assert_eq!(scopes_for(&with_scopes), "user:inference");

        let empty = parse_claude_code_credentials(
            &json!({ "accessToken": "a", "scopes": [] }),
            Path::new("/x"),
        )
        .unwrap();
        assert_eq!(scopes_for(&empty), DEFAULT_SCOPES);

        let none =
            parse_claude_code_credentials(&json!({ "accessToken": "a" }), Path::new("/x")).unwrap();
        assert_eq!(scopes_for(&none), DEFAULT_SCOPES);
    }

    #[test]
    fn build_refreshed_keeps_refresh_and_scopes_when_response_omits_them() {
        let old = parse_claude_code_credentials(
            &json!({ "accessToken": "a", "refreshToken": "old-refresh", "scopes": ["keep"] }),
            Path::new("/x"),
        )
        .unwrap();

        let kept = build_refreshed(
            &old,
            &RefreshResponse {
                access: "new".to_string(),
                refresh: None,
                expires_in_secs: 3600,
                scope: None,
            },
            NOW,
        );
        assert_eq!(kept.access, "new");
        assert_eq!(kept.refresh, "old-refresh");
        assert_eq!(kept.expires, NOW + 3600 * 1000);
        assert_eq!(kept.extra.get("scopes"), Some(&json!(["keep"])));

        let rotated = build_refreshed(
            &old,
            &RefreshResponse {
                access: "new".to_string(),
                refresh: Some("rotated".to_string()),
                expires_in_secs: 60,
                scope: Some("s1 s2".to_string()),
            },
            NOW,
        );
        assert_eq!(rotated.refresh, "rotated");
        assert_eq!(rotated.extra.get("scopes"), Some(&json!(["s1", "s2"])));
    }

    #[test]
    fn parse_refresh_response_requires_access_and_expiry() {
        assert!(parse_refresh_response(&json!({ "expires_in": 1 })).is_err());
        assert!(parse_refresh_response(&json!({ "access_token": "a" })).is_err());
        let ok = parse_refresh_response(&json!({ "access_token": "a", "expires_in": 5 })).unwrap();
        assert_eq!(ok.access, "a");
        assert_eq!(ok.expires_in_secs, 5);
        assert!(ok.refresh.is_none());
    }

    // ---- load precedence & file fallback ------------------------------------

    #[test]
    fn iris_store_credentials_take_precedence_over_claude_code_file() -> Result<()> {
        let dir = unique_dir("precedence");
        let store = store_without_iris_creds(&dir);
        store.storage.set_oauth_credentials(
            AUTH_PROVIDER,
            OAuthCredentials {
                access: "iris-store-token".to_string(),
                refresh: "iris-refresh".to_string(),
                expires: NOW + HOUR_MS,
                extra: Map::new(),
            },
        )?;
        let path = dir.join(".credentials.json");
        write_credentials(&path, &nested_credentials(json!({})));

        let token = store.resolve_with(
            false,
            None,
            &path,
            &seams("linux", &never_security, &never_exchange),
        )?;
        // The Iris store wins; the Claude Code file token is never consulted.
        assert_eq!(token, "iris-store-token");
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn fresh_file_token_is_returned_without_refresh() -> Result<()> {
        let dir = unique_dir("fresh-file");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json");
        write_credentials(
            &path,
            &nested_credentials(json!({ "accessToken": "fresh" })),
        );

        let token = store.resolve_with(
            false,
            None,
            &path,
            &seams("linux", &never_security, &never_exchange),
        )?;
        assert_eq!(token, "fresh");
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn flat_top_level_access_token_is_accepted() -> Result<()> {
        let dir = unique_dir("flat");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json");
        write_credentials(&path, &json!({ "accessToken": "flat-token" }));

        let token = store.resolve_with(
            false,
            None,
            &path,
            &seams("linux", &never_security, &never_exchange),
        )?;
        assert_eq!(token, "flat-token");
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn refreshes_expired_file_token_and_preserves_metadata() -> Result<()> {
        let dir = unique_dir("refresh-file");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json");
        write_credentials(
            &path,
            &nested_credentials(json!({ "expiresAt": NOW - 1, "rateLimitTier": "tier5" })),
        );

        let calls = AtomicUsize::new(0);
        let exchange = |refresh_token: &str, scope: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(refresh_token, "fake-refresh-token");
            assert_eq!(scope, "user:profile user:inference");
            Ok(RefreshResponse {
                access: "refreshed-access".to_string(),
                refresh: Some("refreshed-refresh".to_string()),
                expires_in_secs: 3600,
                scope: Some("user:profile user:inference user:file_upload".to_string()),
            })
        };

        let token = store.resolve_with(
            false,
            None,
            &path,
            &seams("linux", &never_security, &exchange),
        )?;

        assert_eq!(token, "refreshed-access");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let saved = read_oauth(&path);
        assert_eq!(saved["accessToken"], json!("refreshed-access"));
        assert_eq!(saved["refreshToken"], json!("refreshed-refresh"));
        assert_eq!(saved["expiresAt"], json!((NOW + 3600 * 1000) as u64));
        assert_eq!(
            saved["scopes"],
            json!(["user:profile", "user:inference", "user:file_upload"])
        );
        // Unrelated Claude Code metadata must survive the rewrite.
        assert_eq!(saved["subscriptionType"], json!("max"));
        assert_eq!(saved["rateLimitTier"], json!("tier5"));
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn expired_file_without_refresh_token_errors_before_exchange() {
        let dir = unique_dir("no-refresh");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json");
        write_credentials(
            &path,
            &json!({ "claudeAiOauth": { "accessToken": "a", "expiresAt": NOW - 1 } }),
        );

        let error = store
            .resolve_with(
                false,
                None,
                &path,
                &seams("linux", &never_security, &never_exchange),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("refresh token"), "got: {error}");
        assert!(!error.contains("fake-access-token"));
        fs::remove_dir_all(&dir).ok();
    }

    // ---- force refresh -------------------------------------------------------

    #[test]
    fn force_refresh_refreshes_even_a_fresh_token() -> Result<()> {
        let dir = unique_dir("force");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json");
        write_credentials(&path, &nested_credentials(json!({})));

        let exchange = |_: &str, _: &str| {
            Ok(RefreshResponse {
                access: "forced-access".to_string(),
                refresh: Some("forced-refresh".to_string()),
                expires_in_secs: 3600,
                scope: None,
            })
        };
        let token = store.resolve_with(
            true,
            Some("fake-access-token"),
            &path,
            &seams("linux", &never_security, &exchange),
        )?;
        assert_eq!(token, "forced-access");
        assert_eq!(read_oauth(&path)["accessToken"], json!("forced-access"));
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn force_refresh_never_hands_back_the_rejected_token_reintroduced_externally() -> Result<()> {
        let dir = unique_dir("force-rejected");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json");
        // On-disk token differs from the rejected one and is near expiry, so the
        // entry coalescing check does not short-circuit.
        write_credentials(
            &path,
            &nested_credentials(json!({ "accessToken": "stale-token", "expiresAt": NOW - 1 })),
        );
        let path_for_exchange = path.clone();

        // During the exchange, another writer reintroduces the rejected token as
        // a "fresh" (future-expiry) credential.
        let exchange = move |_: &str, _: &str| {
            write_credentials(
                &path_for_exchange,
                &nested_credentials(json!({
                    "accessToken": "rejected-token",
                    "expiresAt": NOW + 2 * HOUR_MS,
                })),
            );
            Ok(RefreshResponse {
                access: "our-refreshed".to_string(),
                refresh: Some("our-refresh".to_string()),
                expires_in_secs: 3600,
                scope: None,
            })
        };

        let token = store.resolve_with(
            true,
            Some("rejected-token"),
            &path,
            &seams("linux", &never_security, &exchange),
        )?;
        // The rejected token must never be returned; our fresh refresh wins and
        // overwrites the externally reintroduced bad token.
        assert_eq!(token, "our-refreshed");
        assert_eq!(read_oauth(&path)["accessToken"], json!("our-refreshed"));
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    // ---- stale-write avoidance ----------------------------------------------

    #[test]
    fn does_not_overwrite_a_newer_token_written_during_refresh() -> Result<()> {
        let dir = unique_dir("stale-write");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json");
        write_credentials(&path, &nested_credentials(json!({ "expiresAt": NOW - 1 })));
        let path_for_exchange = path.clone();

        // During our exchange, another process writes a fresh token to the file.
        let exchange = move |_: &str, _: &str| {
            write_credentials(
                &path_for_exchange,
                &nested_credentials(json!({
                    "accessToken": "external-token",
                    "refreshToken": "external-refresh",
                    "expiresAt": NOW + 2 * HOUR_MS,
                })),
            );
            Ok(RefreshResponse {
                access: "our-late-token".to_string(),
                refresh: Some("our-late-refresh".to_string()),
                expires_in_secs: 3600,
                scope: None,
            })
        };

        let token = store.resolve_with(
            false,
            None,
            &path,
            &seams("linux", &never_security, &exchange),
        )?;
        // We must hand back the externally-written fresh token, not clobber it.
        assert_eq!(token, "external-token");
        let saved = read_oauth(&path);
        assert_eq!(saved["accessToken"], json!("external-token"));
        assert_eq!(saved["refreshToken"], json!("external-refresh"));
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    // ---- refresh coalescing --------------------------------------------------

    #[test]
    fn concurrent_refreshes_for_the_same_source_coalesce() -> Result<()> {
        let dir = unique_dir("coalesce");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json");
        write_credentials(&path, &nested_credentials(json!({ "expiresAt": NOW - 1 })));

        let calls = AtomicUsize::new(0);
        // Gate the first exchange open until both threads are contending, so the
        // second is provably blocked on the lock (not racing ahead).
        let release = Mutex::new(false);
        let release_cv = Condvar::new();

        let outcomes = std::thread::scope(|scope| {
            let run = || {
                let exchange = |_: &str, _: &str| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let mut released = release.lock().unwrap();
                    while !*released {
                        released = release_cv.wait(released).unwrap();
                    }
                    Ok(RefreshResponse {
                        access: "coalesced-access".to_string(),
                        refresh: Some("coalesced-refresh".to_string()),
                        expires_in_secs: 3600,
                        scope: None,
                    })
                };
                store.resolve_with(
                    false,
                    None,
                    &path,
                    &seams("linux", &never_security, &exchange),
                )
            };
            let first = scope.spawn(run);
            let second = scope.spawn(run);

            // Wait for exactly one thread to enter the exchange, then confirm the
            // other is parked on the lock before releasing.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while calls.load(Ordering::SeqCst) == 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "exchange never started"
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "second refresh must wait on the lock, not start a duplicate exchange"
            );
            *release.lock().unwrap() = true;
            release_cv.notify_all();

            (first.join().unwrap(), second.join().unwrap())
        });

        let first = outcomes.0?;
        let second = outcomes.1?;
        assert_eq!(first, "coalesced-access");
        assert_eq!(second, "coalesced-access");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only one token exchange for two concurrent refreshes"
        );
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    // ---- macOS Keychain fallback --------------------------------------------

    #[test]
    fn macos_keychain_fallback_loads_when_file_is_absent() -> Result<()> {
        let dir = unique_dir("keychain");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json"); // intentionally absent
        let blob =
            serde_json::to_string(&nested_credentials(json!({ "accessToken": "kc-token" })))?;
        let calls = AtomicUsize::new(0);
        let run_security = |args: &[&str]| {
            calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                args,
                [
                    "find-generic-password",
                    "-s",
                    "Claude Code-credentials",
                    "-w"
                ]
            );
            Ok(blob.clone())
        };

        let token = store.resolve_with(
            false,
            None,
            &path,
            &seams("macos", &run_security, &never_exchange),
        )?;
        assert_eq!(token, "kc-token");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn keychain_is_never_consulted_on_non_macos() {
        let dir = unique_dir("no-keychain");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json"); // absent

        let error = store
            .resolve_with(
                false,
                None,
                &path,
                &seams("linux", &never_security, &never_exchange),
            )
            .unwrap_err()
            .to_string();
        assert!(error.to_lowercase().contains("claude"), "got: {error}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refreshed_keychain_credential_is_persisted_to_the_file_path() -> Result<()> {
        let dir = unique_dir("keychain-refresh");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json"); // absent: Keychain is the source
        let blob = serde_json::to_string(&nested_credentials(json!({ "expiresAt": NOW - 1 })))?;
        let run_security = |_: &[&str]| Ok(blob.clone());
        let exchange = |refresh_token: &str, _: &str| {
            assert_eq!(refresh_token, "fake-refresh-token");
            Ok(RefreshResponse {
                access: "kc-refreshed".to_string(),
                refresh: None, // server omitted a rotated token: keep the old one
                expires_in_secs: 1800,
                scope: None,
            })
        };

        let token = store.resolve_with(
            false,
            None,
            &path,
            &seams("macos", &run_security, &exchange),
        )?;
        assert_eq!(token, "kc-refreshed");
        // The refreshed Keychain credential is written to the standard file path.
        assert!(
            path.exists(),
            "refreshed keychain credential must land on disk"
        );
        let saved = read_oauth(&path);
        assert_eq!(saved["accessToken"], json!("kc-refreshed"));
        assert_eq!(saved["refreshToken"], json!("fake-refresh-token"));
        assert_eq!(saved["expiresAt"], json!((NOW + 1800 * 1000) as u64));
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    // ---- redaction -----------------------------------------------------------

    #[test]
    fn malformed_file_error_is_redacted_with_login_hint() {
        let dir = unique_dir("malformed");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json");
        let secret = "secret-blob-xyz-must-not-leak";
        fs::write(&path, format!("{{ not json {secret}")).unwrap();

        let error = store
            .resolve_with(
                false,
                None,
                &path,
                &seams("linux", &never_security, &never_exchange),
            )
            .unwrap_err()
            .to_string();
        assert!(error.to_lowercase().contains("claude"), "got: {error}");
        assert!(
            !error.contains(secret),
            "must not leak file contents: {error}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refresh_http_failure_does_not_leak_response_body_or_tokens() {
        let dir = unique_dir("http-fail");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json");
        write_credentials(&path, &nested_credentials(json!({ "expiresAt": NOW - 1 })));
        let exchange = |_: &str, _: &str| -> Result<RefreshResponse> {
            // Production drops the body before reaching here; assert callers see
            // no secret if the exchange itself surfaces one.
            Err(anyhow!(
                "Claude Code OAuth token refresh failed with HTTP 400"
            ))
        };

        let error = store
            .resolve_with(
                false,
                None,
                &path,
                &seams("linux", &never_security, &exchange),
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("refresh failed with HTTP 400"),
            "got: {error}"
        );
        assert!(!error.contains("fake-refresh-token"));
        assert!(!error.contains("fake-access-token"));
    }

    #[test]
    fn keychain_runner_error_is_sanitized() {
        let dir = unique_dir("kc-error");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json"); // absent
        let raw = "security: SecKeychainSearchCopyNext: item could not be found";
        let run_security = |_: &[&str]| Err(anyhow!("{raw}"));

        let error = store
            .resolve_with(
                false,
                None,
                &path,
                &seams("macos", &run_security, &never_exchange),
            )
            .unwrap_err()
            .to_string();
        assert!(error.to_lowercase().contains("keychain"), "got: {error}");
        assert!(
            !error.contains(raw),
            "must not expose raw security output: {error}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keychain_malformed_blob_is_redacted() {
        let dir = unique_dir("kc-blob");
        let store = store_without_iris_creds(&dir);
        let path = dir.join(".credentials.json"); // absent
        let secret = "keychain-secret-blob-must-not-leak";
        let run_security = |_: &[&str]| Ok(format!("{{ bad {secret}"));

        let error = store
            .resolve_with(
                false,
                None,
                &path,
                &seams("macos", &run_security, &never_exchange),
            )
            .unwrap_err()
            .to_string();
        assert!(error.to_lowercase().contains("claude"), "got: {error}");
        assert!(
            !error.contains(secret),
            "must not leak keychain blob: {error}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    // ---- file write shape (kept from the original module) -------------------

    #[test]
    fn write_back_keeps_flat_shape_and_permissions() -> Result<()> {
        let dir = unique_dir("flat-write");
        let path = dir.join(".credentials.json");
        write_credentials(
            &path,
            &json!({ "accessToken": "old", "refreshToken": "old", "expiresAt": 1_u64, "scopes": ["x"] }),
        );
        let creds = OAuthCredentials {
            access: "new-acc".to_string(),
            refresh: "new-ref".to_string(),
            expires: 1_800_000_000_000,
            extra: Map::new(),
        };

        write_claude_code_file(&path, &creds)?;

        let back: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        assert!(back.get("claudeAiOauth").is_none(), "flat shape preserved");
        assert_eq!(back["accessToken"], json!("new-acc"));
        assert_eq!(back["refreshToken"], json!("new-ref"));
        // No scopes in extra -> the file's existing scopes are left untouched.
        assert_eq!(back["scopes"], json!(["x"]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        }
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn unique_tmp_path_is_not_the_static_tmp_sibling() {
        let path = Path::new("/tmp/.credentials.json");
        let tmp = unique_tmp_path(path);
        assert_ne!(tmp, path.with_extension("tmp"));
        assert!(
            tmp.extension()
                .unwrap()
                .to_string_lossy()
                .starts_with("tmp-")
        );
    }
}
