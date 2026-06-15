//! Iris settings file: a focused JSON config for provider/model defaults.
//!
//! Mirrors pi's settings model (`~/.pi/agent/settings.json` +
//! `.pi/settings.json`, project overriding global). Iris keeps its config under
//! the same `~/.iris` directory as the auth file:
//!
//! | Location                  | Scope                       |
//! | ------------------------- | --------------------------- |
//! | `~/.iris/settings.json`   | Global (all projects)       |
//! | `<cwd>/.iris/settings.json` | Project (current directory) |
//!
//! Project settings override global settings field-by-field. Explicit runtime
//! input still wins over the file: the provider applies `env > settings >
//! built-in default` (see `OpenAiCodexResponsesConfig::resolve`). Unknown keys
//! are ignored so older binaries tolerate newer config. A malformed file is a
//! hard error -- a silently-ignored config is a footgun.
//!
//! Tool/approval policy is intentionally not configured here: pi's settings do
//! not encode tool-execution policy either, and cross-session approval
//! persistence is tracked separately (roadmap #14).

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Settings loaded from the JSON config files. Every field is optional; an
/// absent field falls back to the next layer (project -> global -> built-in
/// default, with env applied above all by the provider).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Settings {
    /// Provider id (only `openai-codex` is supported today). Validated by the
    /// caller so an unsupported value fails loudly rather than silently.
    pub(crate) default_provider: Option<String>,
    /// Model id passed to the active provider.
    pub(crate) default_model: Option<String>,
    /// Base URL override for the Codex provider.
    pub(crate) base_url: Option<String>,
}

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

    /// Field-by-field merge where `project` wins over `self` (global).
    fn merged_with(self, project: Settings) -> Settings {
        Settings {
            default_provider: project.default_provider.or(self.default_provider),
            default_model: project.default_model.or(self.default_model),
            base_url: project.base_url.or(self.base_url),
        }
    }
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
    fn project_overrides_global_field_by_field() {
        let dir = temp_dir();
        let global = dir.path.join("global.json");
        let project = dir.path.join("project.json");
        fs::write(
            &global,
            r#"{ "defaultModel": "global-model", "baseUrl": "https://global.example" }"#,
        )
        .unwrap();
        // Project overrides the model but leaves baseUrl to fall through to global.
        fs::write(&project, r#"{ "defaultModel": "project-model" }"#).unwrap();

        let settings = Settings::load_from(Some(&global), &project).unwrap();
        assert_eq!(settings.default_model.as_deref(), Some("project-model"));
        assert_eq!(settings.base_url.as_deref(), Some("https://global.example"));
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
