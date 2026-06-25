//! Iris settings file: a focused JSON config for provider/model defaults.
//!
//! Mirrors pi's settings model (`~/.pi/agent/settings.json` +
//! `.pi/settings.json`) with one security caveat: untrusted project-local config
//! may override the model only, while provider and base-url selection come from
//! global/user config. Iris keeps its config under the same `~/.iris` directory
//! as the auth file:
//!
//! | Location                  | Scope                       |
//! | ------------------------- | --------------------------- |
//! | `~/.iris/settings.json`   | Global (all projects)       |
//! | `<cwd>/.iris/settings.json` | Project (current directory) |
//!
//! Project settings override the model only. Provider/base-url are intentionally
//! user-global so a cloned repository cannot redirect OAuth bearer tokens to a
//! malicious endpoint. Explicit runtime input still wins over the file where a
//! provider supports env overrides. Unknown keys are ignored so older binaries
//! tolerate newer config. A malformed file is a hard error -- a silently-ignored
//! config is a footgun.
//!
//! Tool/approval policy is intentionally not configured here: pi's settings do
//! not encode tool-execution policy either, and cross-session approval
//! persistence is tracked separately (roadmap #14).

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

/// Settings loaded from the JSON config files. Every field is optional; an
/// absent field falls back to the next layer (safe project fields -> global ->
/// built-in default, with env applied above where the provider supports it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Settings {
    /// Provider id: `openai-codex` (default), `anthropic`, or `antigravity`.
    /// Validated by the caller (`build_provider`) so an unsupported value fails
    /// loudly rather than silently.
    pub(crate) default_provider: Option<String>,
    /// Model id passed to the active provider.
    pub(crate) default_model: Option<String>,
    /// Base URL override for the active provider's API endpoint.
    pub(crate) base_url: Option<String>,
    /// Context token budget threshold. The Tier-2 harness reads it to decide
    /// when to auto-compact: when the rebuilt/current context token total
    /// exceeds this, the harness compacts at a safe turn boundary. Absent ->
    /// [`Settings::context_token_budget`] default.
    pub(crate) context_token_budget: Option<u64>,
    /// Default reasoning/thinking effort (`off|minimal|low|medium|high|xhigh`),
    /// parsed into a normalized level by `mimir::selection`. Absent -> no
    /// preference, so adapters omit all reasoning fields (today's wire). Not a
    /// security-sensitive redirect, so a project file may tune it (like
    /// [`Settings::context_token_budget`]).
    pub(crate) default_reasoning: Option<String>,
    /// Prompt cache retention (`none|short|long`). Global-only because a cloned
    /// project must not silently increase how long provider-side prompt material
    /// may live, or add cache-write cost, without the user opting in. Parsed by
    /// `mimir::selection`; absent -> `none` (off, byte-identical request).
    pub(crate) prompt_cache_retention: Option<String>,
    /// Anthropic server-side context-management opt-in
    /// (`context_management.edits`). Stored as a raw JSON object and parsed into
    /// typed edits by `mimir::selection`; absent/empty -> disabled (no
    /// `context_management` field, no extra betas). Global-only: server-side
    /// context edits change request behavior and cost, so an untrusted project
    /// file must not enable them.
    pub(crate) anthropic_context_management: Option<Value>,
    /// Ordered `provider/model` ids that scope Ctrl+P cycling (the persisted
    /// `/scoped-models` selection). Like provider/base-url this controls which
    /// providers a session talks to, so it is global-only: a cloned repo cannot
    /// silently change the cycle scope.
    pub(crate) enabled_models: Option<Vec<String>>,
    /// Optional graceful soft cap on tool round-trips per turn. Absent (the
    /// default) leaves the agent loop unbounded: it runs while the model emits
    /// tool calls and stops naturally, with cancellation as the runaway guard.
    /// When set, the loop ends the turn with a Notice after this many
    /// round-trips. Not a security redirect, so a project may tune it (like
    /// [`Settings::context_token_budget`]); a project value can only narrow a
    /// run, never redirect tokens.
    pub(crate) max_tool_roundtrips: Option<usize>,
    /// Provider retry/backoff tuning (max retries, base/max backoff). Absent
    /// subfields fall back to the built-in defaults via
    /// [`Settings::retry_settings`]. Global-only: retry volume affects provider
    /// request load and cost, so an untrusted project file must not crank it up
    /// (same reasoning as `prompt_cache_retention`).
    pub(crate) retry: Option<RetrySettings>,
}

/// Raw provider retry/backoff config (all fields optional). Resolved into the
/// shared `mimir` retry policy by [`Settings::retry_settings`] +
/// [`crate::mimir::selection::ModelSelection::resolve`], which fills any absent
/// subfield with the built-in default. Kept as plain config data here so the
/// settings layer does not depend on the provider transport.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RetrySettings {
    /// Maximum transient retries before giving up.
    pub(crate) max_retries: Option<u32>,
    /// Base backoff in milliseconds, doubled per retry.
    pub(crate) base_delay_ms: Option<u64>,
    /// Backoff ceiling in milliseconds.
    pub(crate) max_delay_ms: Option<u64>,
}

/// Default context token budget when none is configured. A conservative ceiling
/// that fits common model context windows; it is only surfaced through
/// [`Settings::context_token_budget`] and triggers nothing yet.
const DEFAULT_CONTEXT_TOKEN_BUDGET: u64 = 128_000;

impl Settings {
    /// Load and merge the global and project settings files for `cwd`.
    pub(crate) fn load(cwd: &Path) -> Result<Self> {
        Self::load_from(global_path().as_deref(), &project_path(cwd))
    }

    /// Core loader, split out so tests can supply explicit file paths. A
    /// missing file (or absent global path) contributes nothing; a
    /// present-but-malformed file errors.
    fn load_from(global: Option<&Path>, project: &Path) -> Result<Self> {
        let global = match global {
            Some(path) => read_optional(path)?.unwrap_or_default(),
            None => Settings::default(),
        };
        let project = read_optional(project)?.unwrap_or_default();
        Ok(global.merged_with(project))
    }

    /// Merge project settings into global settings. Project config is usually
    /// repo-controlled, so only model choice is trusted there; provider and
    /// base-url control where bearer tokens are sent and must come from global
    /// user config or built-in defaults.
    fn merged_with(self, project: Settings) -> Settings {
        Settings {
            default_provider: self.default_provider,
            default_model: project.default_model.or(self.default_model),
            base_url: self.base_url,
            // A budget is not a security-sensitive redirect (unlike provider /
            // base-url), so a project may tune it; fall back to global, then the
            // built-in default via the accessor.
            context_token_budget: project.context_token_budget.or(self.context_token_budget),
            // Reasoning effort is likewise not a security redirect, so a project
            // may override it; fall back to global.
            default_reasoning: project.default_reasoning.or(self.default_reasoning),
            // Prompt cache retention can affect privacy/cost, so keep it
            // global-only like provider/base-url and scoped model sets.
            prompt_cache_retention: self.prompt_cache_retention,
            // Context management changes request behavior/cost server-side, so
            // it is likewise global-only and never taken from project config.
            anthropic_context_management: self.anthropic_context_management,
            // Scoped models gate which providers a session cycles through, so
            // (like provider/base-url) they are global-only and never taken from
            // untrusted project config.
            enabled_models: self.enabled_models,
            // A turn cap can only narrow a run (never redirect tokens), so a
            // project may tune it; fall back to global.
            max_tool_roundtrips: project.max_tool_roundtrips.or(self.max_tool_roundtrips),
            // Retry tuning affects provider load/cost, so keep it global-only
            // like prompt cache retention; never taken from project config.
            retry: self.retry,
        }
    }

    /// Configured tool round-trip soft cap, or `None` (unbounded) when unset.
    /// The host installs it on the agent so a reached cap ends the turn with a
    /// graceful Notice instead of a fatal error.
    pub(crate) fn max_tool_roundtrips(&self) -> Option<usize> {
        self.max_tool_roundtrips
    }

    /// Raw retry settings (or an all-default empty set when unconfigured). The
    /// selection layer resolves these into the shared retry policy, filling any
    /// absent subfield with the built-in default.
    pub(crate) fn retry_settings(&self) -> RetrySettings {
        self.retry.clone().unwrap_or_default()
    }

    /// Configured context token budget, or the built-in default when unset. The
    /// harness compares the current context token total against this and
    /// auto-compacts when it is exceeded.
    pub(crate) fn context_token_budget(&self) -> u64 {
        self.context_token_budget
            .unwrap_or(DEFAULT_CONTEXT_TOKEN_BUDGET)
    }
}

/// Persist `provider`/`model` as the default model in the global settings file,
/// preserving every other key. Written to the user-global file (never project
/// config) so it is consistent with the global-only provider security rule.
pub(crate) fn save_default_model(provider: &str, model: &str) -> Result<()> {
    update_global(&[
        ("defaultProvider", Value::String(provider.to_string())),
        ("defaultModel", Value::String(model.to_string())),
    ])
}

/// The effective default model as a canonical `provider/model` id. The `/model`
/// picker uses it to label the "Default" row, which can differ from the active
/// session model after a session-only switch. Resolved through
/// [`ModelSelection::resolve`] over the global settings so it applies the same
/// provider/model precedence and built-in fallbacks as startup (and canonicalizes
/// the provider, so a hand-edited `defaultProvider` casing still matches the
/// catalog's lowercase qualified ids). Global-only, matching where
/// `save_default_model` writes; `None` only if the global path is unreadable or
/// settings are invalid.
pub(crate) fn default_model_qualified() -> Option<String> {
    let path = global_path()?;
    let settings = read_optional(&path).ok().flatten().unwrap_or_default();
    let resolved = crate::mimir::selection::ModelSelection::resolve(&settings).ok()?;
    Some(format!("{}/{}", resolved.provider.as_str(), resolved.model))
}

/// Persist the default reasoning/thinking level in the global settings file.
pub(crate) fn save_default_reasoning(level: &str) -> Result<()> {
    update_global(&[("defaultReasoning", Value::String(level.to_string()))])
}

/// Persist (or clear) the scoped-model cycle set in the global settings file.
/// `None` (or an empty list) removes `enabledModels` so the session cycles all
/// authenticated models again, matching pi-mono's Ctrl+S "all enabled" path.
pub(crate) fn save_enabled_models(ids: Option<&[String]>) -> Result<()> {
    let value = match ids {
        Some(ids) if !ids.is_empty() => {
            Value::Array(ids.iter().cloned().map(Value::String).collect())
        }
        // Null is the documented "remove this key" sentinel for `update_global`.
        _ => Value::Null,
    };
    update_global(&[("enabledModels", value)])
}

/// Read the global settings file as a raw JSON object, apply `updates` (a
/// `Value::Null` removes the key), and write it back atomically. Reading the raw
/// map -- rather than reserializing the typed [`Settings`] -- preserves any keys
/// this binary does not know about, so an older Iris never drops a newer config
/// field. A missing file starts from an empty object.
fn update_global(updates: &[(&str, Value)]) -> Result<()> {
    let path = global_path()
        .context("cannot resolve the global settings path (set HOME or IRIS_CONFIG_PATH)")?;
    let mut object = read_object(&path)?;
    for (key, value) in updates {
        if value.is_null() {
            object.remove(*key);
        } else {
            object.insert((*key).to_string(), value.clone());
        }
    }
    write_object_atomically(&path, &object)
}

/// Read a settings file as a JSON object, returning an empty object when the
/// file is absent. A present-but-non-object file is an error rather than a
/// silent overwrite, so a hand-edited config is never clobbered blindly.
fn read_object(path: &Path) -> Result<Map<String, Value>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str(&contents)
            .with_context(|| format!("invalid settings file {}", path.display()))?
        {
            Value::Object(object) => Ok(object),
            _ => bail!("settings file {} is not a JSON object", path.display()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Write the settings object to `path` via temp-file + rename so a crash never
/// leaves a half-written config.
fn write_object_atomically(path: &Path, object: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(object)?;
    let tmp = path.with_extension(format!(
        "tmp-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut file = std::fs::File::create(&tmp)
        .with_context(|| format!("failed to create {}", tmp.display()))?;
    // fsync before the rename: the rename is atomic, but without flushing the
    // file's data first a crash right after rename can expose a zero-length or
    // partially written settings.json.
    file.write_all(raw.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))
}

/// Parse a settings file, returning `None` when it does not exist.
fn read_optional(path: &Path) -> Result<Option<Settings>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let settings = serde_json::from_str(&contents)
        .with_context(|| format!("invalid settings file {}", path.display()))?;
    Ok(Some(settings))
}

/// Global settings path: `IRIS_CONFIG_PATH` override, else `~/.iris/settings.json`.
/// Mirrors the `IRIS_AUTH_PATH` / `~/.iris/auth.json` convention. Returns `None`
/// when neither `IRIS_CONFIG_PATH` nor `HOME` is set, so a missing `HOME` skips
/// the global layer rather than resolving to a relative `.iris/settings.json`
/// that would double-read the project file.
fn global_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("IRIS_CONFIG_PATH") {
        return Some(PathBuf::from(path));
    }
    let home = env::var("HOME").ok().filter(|home| !home.is_empty())?;
    Some(Path::new(&home).join(".iris/settings.json"))
}

fn project_path(cwd: &Path) -> PathBuf {
    cwd.join(".iris/settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir {
        path: PathBuf,
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn temp_dir() -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("iris-config-test-{nanos}-{seq}"));
        fs::create_dir(&path).unwrap();
        TempDir { path }
    }

    #[test]
    fn missing_files_yield_default_settings() {
        let dir = temp_dir();
        let settings = Settings::load_from(
            Some(&dir.path.join("nope.json")),
            &dir.path.join("also-nope.json"),
        )
        .unwrap();
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn absent_global_path_loads_only_project() {
        let dir = temp_dir();
        let project = dir.path.join("project.json");
        fs::write(&project, r#"{ "defaultModel": "project-model" }"#).unwrap();
        // None global path models a missing HOME: the project file still loads.
        let settings = Settings::load_from(None, &project).unwrap();
        assert_eq!(settings.default_model.as_deref(), Some("project-model"));
    }

    #[test]
    fn project_overrides_only_model() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(
            &global,
            r#"{ "defaultProvider": "openai-codex", "defaultModel": "global-model", "baseUrl": "https://global.example" }"#,
        )
        .unwrap();
        fs::write(
            &project,
            r#"{ "defaultProvider": "antigravity", "defaultModel": "project-model", "baseUrl": "https://evil.example" }"#,
        )
        .unwrap();

        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert_eq!(settings.default_provider.as_deref(), Some("openai-codex"));
        assert_eq!(settings.default_model.as_deref(), Some("project-model"));
        assert_eq!(settings.base_url.as_deref(), Some("https://global.example"));
    }

    #[test]
    fn project_may_override_reasoning_but_not_provider_or_base_url() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(
            &global,
            r#"{ "defaultProvider": "openai-codex", "baseUrl": "https://global.example", "defaultReasoning": "low" }"#,
        )
        .unwrap();
        // A malicious project tries to redirect provider/base-url AND tune
        // reasoning. Only reasoning (and model, tested above) is trusted there.
        fs::write(
            &project,
            r#"{ "defaultProvider": "antigravity", "baseUrl": "https://evil.example", "defaultReasoning": "high" }"#,
        )
        .unwrap();

        let settings = Settings::load_from(Some(&global), &project).unwrap();
        // Security invariant: provider/base-url stay global-only.
        assert_eq!(settings.default_provider.as_deref(), Some("openai-codex"));
        assert_eq!(settings.base_url.as_deref(), Some("https://global.example"));
        // Reasoning is not a redirect, so the project override wins.
        assert_eq!(settings.default_reasoning.as_deref(), Some("high"));
    }

    #[test]
    fn context_token_budget_defaults_when_unset_and_parses_when_present() {
        let dir = temp_dir();
        // Unset -> built-in default, no error.
        let defaulted = Settings::load_from(
            Some(&dir.path.join("none.json")),
            &dir.path.join("none.json"),
        )
        .unwrap();
        assert_eq!(defaulted.context_token_budget, None);
        assert_eq!(
            defaulted.context_token_budget(),
            DEFAULT_CONTEXT_TOKEN_BUDGET
        );

        // Present -> parsed and surfaced; a project may tune it.
        let project = dir.path.join("project.json");
        fs::write(&project, r#"{ "contextTokenBudget": 64000 }"#).unwrap();
        let configured = Settings::load_from(None, &project).unwrap();
        assert_eq!(configured.context_token_budget, Some(64_000));
        assert_eq!(configured.context_token_budget(), 64_000);
    }

    #[test]
    fn max_tool_roundtrips_defaults_to_unbounded_and_a_project_may_set_it() {
        let dir = temp_dir();
        // Unset -> None (unbounded loop).
        let defaulted = Settings::load_from(
            Some(&dir.path.join("none.json")),
            &dir.path.join("none.json"),
        )
        .unwrap();
        assert_eq!(defaulted.max_tool_roundtrips(), None);

        // A project may set the soft cap (it can only narrow a run).
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(&global, r#"{ "maxToolRoundtrips": 100 }"#).unwrap();
        fs::write(&project, r#"{ "maxToolRoundtrips": 20 }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert_eq!(settings.max_tool_roundtrips(), Some(20));
    }

    #[test]
    fn retry_settings_parse_and_default_and_are_global_only() {
        let dir = temp_dir();
        // Unset -> an all-default (empty) raw set.
        let defaulted = Settings::load_from(
            Some(&dir.path.join("none.json")),
            &dir.path.join("none.json"),
        )
        .unwrap();
        assert_eq!(defaulted.retry_settings(), RetrySettings::default());

        // Present -> parsed; global-only (a cloned project cannot crank it up).
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(
            &global,
            r#"{ "retry": { "maxRetries": 5, "baseDelayMs": 1000, "maxDelayMs": 30000 } }"#,
        )
        .unwrap();
        fs::write(
            &project,
            r#"{ "retry": { "maxRetries": 99, "baseDelayMs": 1 } }"#,
        )
        .unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        let retry = settings.retry_settings();
        assert_eq!(retry.max_retries, Some(5));
        assert_eq!(retry.base_delay_ms, Some(1000));
        assert_eq!(retry.max_delay_ms, Some(30000));
    }

    #[test]
    fn prompt_cache_retention_is_global_only() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(&global, r#"{ "promptCacheRetention": "none" }"#).unwrap();
        fs::write(&project, r#"{ "promptCacheRetention": "long" }"#).unwrap();

        let settings = Settings::load_from(Some(&global), &project).unwrap();

        assert_eq!(settings.prompt_cache_retention.as_deref(), Some("none"));
    }

    #[test]
    fn anthropic_context_management_is_global_only() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(
            &global,
            r#"{ "anthropicContextManagement": { "clearThinking": {} } }"#,
        )
        .unwrap();
        // A cloned project must not enable server-side context edits.
        fs::write(
            &project,
            r#"{ "anthropicContextManagement": { "clearToolUses": { "keepToolUses": 1 } } }"#,
        )
        .unwrap();

        let settings = Settings::load_from(Some(&global), &project).unwrap();

        assert_eq!(
            settings.anthropic_context_management,
            Some(serde_json::json!({ "clearThinking": {} }))
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        fs::write(
            &global,
            r#"{ "defaultModel": "m", "futureKnob": 42, "nested": { "x": 1 } }"#,
        )
        .unwrap();
        let settings = Settings::load_from(Some(&global), &dir.path.join("nope.json")).unwrap();
        assert_eq!(settings.default_model.as_deref(), Some("m"));
    }

    #[test]
    fn update_global_preserves_unknown_keys_and_round_trips() {
        let dir = temp_dir();
        let path = dir.path.join("settings.json");
        std::fs::write(&path, r#"{ "futureKnob": 42, "defaultModel": "old" }"#).unwrap();

        // Set known keys; the unknown key must survive.
        let mut object = read_object(&path).unwrap();
        object.insert(
            "defaultProvider".to_string(),
            Value::String("anthropic".to_string()),
        );
        object.insert(
            "defaultModel".to_string(),
            Value::String("claude-sonnet-4-6".to_string()),
        );
        write_object_atomically(&path, &object).unwrap();

        let reread = read_object(&path).unwrap();
        assert_eq!(reread.get("futureKnob"), Some(&Value::from(42)));
        assert_eq!(
            reread.get("defaultProvider"),
            Some(&Value::String("anthropic".to_string()))
        );
        assert_eq!(
            reread.get("defaultModel"),
            Some(&Value::String("claude-sonnet-4-6".to_string()))
        );
        // And the typed loader reads it back.
        let settings = Settings::load_from(Some(&path), &dir.path.join("none.json")).unwrap();
        assert_eq!(settings.default_provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn read_object_missing_file_is_empty_and_non_object_errors() {
        let dir = temp_dir();
        assert!(
            read_object(&dir.path.join("absent.json"))
                .unwrap()
                .is_empty()
        );
        let path = dir.path.join("array.json");
        std::fs::write(&path, "[1, 2, 3]").unwrap();
        let err = read_object(&path).unwrap_err().to_string();
        assert!(err.contains("not a JSON object"), "{err}");
    }

    #[test]
    fn enabled_models_is_global_only() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        std::fs::write(&global, r#"{ "enabledModels": ["openai-codex/gpt-5.5"] }"#).unwrap();
        // A project file cannot inject a cycle scope.
        std::fs::write(&project, r#"{ "enabledModels": ["anthropic/evil"] }"#).unwrap();
        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert_eq!(
            settings.enabled_models.as_deref(),
            Some(["openai-codex/gpt-5.5".to_string()].as_slice())
        );
    }

    #[test]
    fn malformed_file_is_an_error_naming_the_path() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        fs::write(&global, "{ not json").unwrap();
        let err = Settings::load_from(Some(&global), &dir.path.join("nope.json"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid settings file"), "{err}");
        assert!(err.contains("global.json"), "{err}");
    }
}
