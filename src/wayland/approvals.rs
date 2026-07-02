//! Persistent, project-scoped approval grants (Tier 2, issue #209).
//!
//! Allow-always is session-scoped, and `write`/`edit`/`bash` deliberately opt
//! out of it -- every file mutation re-prompts, every session. This store is
//! the durable successor (#14's remaining scope): the user can grant a tool
//! (`write`/`edit`) or an exact bash command for THIS project, and the grant
//! survives across sessions.
//!
//! Security posture:
//! - Grants live in user-local state (`~/.iris/approvals.json`), keyed by the
//!   **canonical** workspace directory -- never in a repo-committed file, so a
//!   cloned repository can never pre-approve itself.
//! - Destructive bash commands (the `rm`/`dd`/`mkfs`/... classification in
//!   `tools::registry`) are never persistable: the gate only consults this
//!   store for calls Nexus classified as non-destructive, and the `[p]` prompt
//!   option is suppressed for destructive calls. That re-prompt floor is the
//!   safety differentiator and keeps its floor.
//! - Bash grants are exact-command matches (whitespace-trimmed), not prefixes:
//!   a grant for `cargo test` does not authorize `cargo test && rm -rf /`.
//!
//! The store path mirrors `trust.rs`: `IRIS_APPROVALS_PATH` override wins, else
//! `~/.iris/approvals.json`. Reads are best-effort and fail closed (no grant).

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

/// One stored grant for a project: a whole tool, or one exact bash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Grant {
    /// Every call to this tool (e.g. `write`, `edit`) is pre-approved.
    Tool(String),
    /// This exact bash command is pre-approved.
    Bash(String),
}

impl Grant {
    /// Stable string key for revocation round-trips (`tool:<name>` /
    /// `bash:<command>`).
    pub(crate) fn key(&self) -> String {
        match self {
            Grant::Tool(name) => format!("tool:{name}"),
            Grant::Bash(command) => format!("bash:{command}"),
        }
    }

    /// Inverse of [`key`](Self::key).
    pub(crate) fn parse(key: &str) -> Option<Grant> {
        if let Some(name) = key.strip_prefix("tool:") {
            return Some(Grant::Tool(name.to_string()));
        }
        key.strip_prefix("bash:")
            .map(|command| Grant::Bash(command.to_string()))
    }

    /// Human-readable row for the review UI.
    pub(crate) fn label(&self) -> String {
        match self {
            Grant::Tool(name) => format!("{name} (all calls)"),
            Grant::Bash(command) => format!("$ {command}"),
        }
    }
}

/// All grants stored for `dir`, tools first then bash commands, each list
/// sorted. Empty on any read/resolve failure (fail closed: no grant).
pub(crate) fn grants_for(dir: &Path) -> Vec<Grant> {
    let Some(store) = store_path() else {
        return Vec::new();
    };
    read_grants(&store, dir)
}

/// Whether a non-destructive call to `tool` (with `bash_command` for the shell
/// tool) is pre-approved for `dir`. Callers must consult this ONLY for calls
/// Nexus classified as non-destructive; the store itself never sees
/// destructiveness.
pub(crate) fn is_granted(dir: &Path, tool: &str, bash_command: Option<&str>) -> bool {
    let grants = grants_for(dir);
    match bash_command {
        Some(command) => {
            let command = command.trim();
            grants
                .iter()
                .any(|grant| matches!(grant, Grant::Bash(stored) if stored == command))
        }
        None => grants
            .iter()
            .any(|grant| matches!(grant, Grant::Tool(name) if name == tool)),
    }
}

/// Persist one grant for `dir` (idempotent).
pub(crate) fn add_grant(dir: &Path, grant: &Grant) -> Result<()> {
    let store = store_path()
        .context("cannot resolve the approvals store path (set HOME or IRIS_APPROVALS_PATH)")?;
    write_grants(&store, dir, |grants| {
        if !grants.contains(grant) {
            grants.push(grant.clone());
        }
    })
}

/// Remove one grant from `dir`'s stored set (no-op when absent).
pub(crate) fn revoke_grant(dir: &Path, grant: &Grant) -> Result<()> {
    let store = store_path()
        .context("cannot resolve the approvals store path (set HOME or IRIS_APPROVALS_PATH)")?;
    write_grants(&store, dir, |grants| {
        grants.retain(|stored| stored != grant);
    })
}

fn read_grants(store: &Path, dir: &Path) -> Vec<Grant> {
    let Some(key) = canonical_key(dir) else {
        return Vec::new();
    };
    let map = read_map(store);
    let Some(entry) = map.get(&key) else {
        return Vec::new();
    };
    let list = |field: &str| -> Vec<String> {
        entry
            .get(field)
            .and_then(Value::as_array)
            .map(|values| {
                let mut list: Vec<String> = values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                list.sort();
                list
            })
            .unwrap_or_default()
    };
    let mut grants: Vec<Grant> = list("tools").into_iter().map(Grant::Tool).collect();
    grants.extend(list("bash").into_iter().map(Grant::Bash));
    grants
}

/// Read-modify-write the grant set for `dir` under `store`.
fn write_grants(store: &Path, dir: &Path, mutate: impl FnOnce(&mut Vec<Grant>)) -> Result<()> {
    let key = canonical_key(dir)
        .with_context(|| format!("cannot canonicalize project directory {}", dir.display()))?;
    let mut grants = read_grants(store, dir);
    mutate(&mut grants);
    let mut tools: Vec<&str> = Vec::new();
    let mut bash: Vec<&str> = Vec::new();
    for grant in &grants {
        match grant {
            Grant::Tool(name) => tools.push(name),
            Grant::Bash(command) => bash.push(command),
        }
    }
    tools.sort_unstable();
    bash.sort_unstable();
    let mut map = read_map(store);
    if tools.is_empty() && bash.is_empty() {
        map.remove(&key);
    } else {
        map.insert(key, json!({ "tools": tools, "bash": bash }));
    }
    write_map_atomically(store, &map)
}

/// Canonical (symlink-resolved) key for `dir`, mirroring `trust.rs`: a
/// symlinked alias must share the real directory's grants, never carry its own.
fn canonical_key(dir: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(dir).ok()?;
    Some(canonical.to_string_lossy().into_owned())
}

/// Read the approvals store as a `{ path: {tools, bash} }` JSON object. A
/// missing file or any parse/shape error yields an empty map (fail closed).
fn read_map(store: &Path) -> Map<String, Value> {
    let Ok(contents) = std::fs::read_to_string(store) else {
        return Map::new();
    };
    match serde_json::from_str(&contents) {
        Ok(Value::Object(object)) => object,
        _ => Map::new(),
    }
}

/// Write the approvals map via temp-file + fsync + rename (mirrors `trust.rs`).
fn write_map_atomically(store: &Path, map: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(map)?;
    let tmp = store.with_extension(format!(
        "tmp-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut file = std::fs::File::create(&tmp)
        .with_context(|| format!("failed to create {}", tmp.display()))?;
    file.write_all(raw.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, store).with_context(|| format!("failed to replace {}", store.display()))
}

/// Approvals store path: `IRIS_APPROVALS_PATH` override, else
/// `~/.iris/approvals.json`. `None` when neither is available.
fn store_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("IRIS_APPROVALS_PATH") {
        return Some(PathBuf::from(path));
    }
    let home = env::var("HOME").ok().filter(|home| !home.is_empty())?;
    Some(Path::new(&home).join(".iris/approvals.json"))
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
        let path = env::temp_dir().join(format!("iris-approvals-test-{nanos}-{seq}"));
        fs::create_dir(&path).unwrap();
        TempDir { path }
    }

    #[test]
    fn grants_round_trip_and_are_project_scoped() {
        let store = temp_dir();
        let store_file = store.path.join("approvals.json");
        let a = temp_dir();
        let b = temp_dir();

        write_grants(&store_file, &a.path, |grants| {
            grants.push(Grant::Tool("edit".into()));
            grants.push(Grant::Bash("cargo test".into()));
        })
        .unwrap();

        let grants = read_grants(&store_file, &a.path);
        assert_eq!(
            grants,
            vec![Grant::Tool("edit".into()), Grant::Bash("cargo test".into())]
        );
        // Another project shares nothing.
        assert!(read_grants(&store_file, &b.path).is_empty());
    }

    #[test]
    fn revoking_the_last_grant_removes_the_project_entry() {
        let store = temp_dir();
        let store_file = store.path.join("approvals.json");
        let project = temp_dir();
        let grant = Grant::Tool("write".into());

        write_grants(&store_file, &project.path, |grants| {
            grants.push(grant.clone())
        })
        .unwrap();
        write_grants(&store_file, &project.path, |grants| {
            grants.retain(|stored| stored != &grant)
        })
        .unwrap();

        assert!(read_grants(&store_file, &project.path).is_empty());
        let raw = fs::read_to_string(&store_file).unwrap();
        let map: Value = serde_json::from_str(&raw).unwrap();
        assert!(
            map.as_object().unwrap().is_empty(),
            "empty project entries are pruned: {raw}"
        );
    }

    #[test]
    fn bash_grants_match_the_exact_command_only() {
        let store = temp_dir();
        let store_file = store.path.join("approvals.json");
        let project = temp_dir();
        write_grants(&store_file, &project.path, |grants| {
            grants.push(Grant::Bash("cargo test".into()))
        })
        .unwrap();

        let grants = read_grants(&store_file, &project.path);
        let matches = |command: &str| {
            grants
                .iter()
                .any(|grant| matches!(grant, Grant::Bash(stored) if stored == command.trim()))
        };
        assert!(matches("cargo test"));
        assert!(matches("  cargo test  "));
        assert!(!matches("cargo test --all"), "prefix must not match");
        assert!(!matches("cargo"), "shorter command must not match");
    }

    #[test]
    fn malformed_store_reads_as_no_grants() {
        let store = temp_dir();
        let store_file = store.path.join("approvals.json");
        fs::write(&store_file, "{ not json").unwrap();
        let project = temp_dir();
        assert!(read_grants(&store_file, &project.path).is_empty());
    }

    #[test]
    fn grant_keys_round_trip() {
        for grant in [
            Grant::Tool("edit".into()),
            Grant::Bash("cargo test --all".into()),
        ] {
            assert_eq!(Grant::parse(&grant.key()), Some(grant));
        }
        assert_eq!(Grant::parse("nonsense"), None);
    }
}
