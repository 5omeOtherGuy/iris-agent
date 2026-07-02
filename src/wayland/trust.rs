//! Project trust gate (Tier 2): decides whether repo-provided Iris resources
//! (system-prompt fragments under `<workspace>/.iris/fragments`) may be folded
//! into the model's instructions.
//!
//! Cloning a hostile repo and running `iris` would otherwise be arbitrary
//! system-prompt injection with zero ceremony: the repo's `.iris/fragments`
//! load unconditionally. This store gates that path. The decision is tri-state
//! ([`TrustDecision`]) and persisted per workspace in `~/.iris/trust.json`,
//! keyed by the **canonical** (symlink-resolved) directory so two paths that
//! resolve to the same directory share one decision and a symlinked alias
//! cannot dodge an untrusted verdict.
//!
//! Security posture:
//! - Only system-prompt-level fragments are gated. Project docs
//!   (`AGENTS.md`/`CLAUDE.md`) keep loading regardless -- they are the same
//!   trust class the harness already reads and are not gated here. Project
//!   settings are already key-restricted (model/reasoning only), so they are
//!   not a redirect vector.
//! - `Undecided` is not `Trusted`: the caller treats an undecided project as
//!   untrusted until an explicit decision is made. Non-interactive contexts
//!   never prompt and never write a decision -- they simply deny.
//! - Both `Trusted` and `Untrusted` are persisted once chosen, so a declined
//!   project stays declined without re-prompting every run.
//!
//! The store path mirrors the `IRIS_AUTH_PATH` / `~/.iris/auth.json` convention:
//! an explicit `IRIS_TRUST_PATH` wins (and does not require `HOME`), else
//! `~/.iris/trust.json`.

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

/// Whether a workspace's repo-provided Iris resources are trusted. `Undecided`
/// is the absence of any recorded decision; callers treat it as untrusted for
/// gating but may prompt for an explicit choice in an interactive context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustDecision {
    Trusted,
    Untrusted,
    Undecided,
}

/// The recorded decision for `dir`, or `Undecided` when none is stored (or the
/// store path / directory cannot be resolved). Reads are best-effort: a missing
/// or malformed store yields `Undecided` rather than an error, so a broken trust
/// file fails closed (deny) instead of crashing startup.
pub(crate) fn decision_for(dir: &Path) -> TrustDecision {
    let Some(store) = store_path() else {
        return TrustDecision::Undecided;
    };
    read_decision(&store, dir)
}

/// Record `trusted` for `dir`, keyed by its canonical path. Persists both the
/// trusted and the untrusted choice (a declined project stays declined). Errors
/// only on a genuine IO failure or when the directory cannot be canonicalized.
pub(crate) fn set_decision(dir: &Path, trusted: bool) -> Result<()> {
    let store = store_path()
        .context("cannot resolve the trust store path (set HOME or IRIS_TRUST_PATH)")?;
    write_decision(&store, dir, trusted)
}

/// Core reader, split out so tests supply an explicit store path. A missing
/// store, an unresolvable directory, or an unknown value all read as
/// `Undecided`.
fn read_decision(store: &Path, dir: &Path) -> TrustDecision {
    let Some(key) = canonical_key(dir) else {
        return TrustDecision::Undecided;
    };
    let map = read_map(store);
    match map.get(&key).and_then(Value::as_str) {
        Some("trusted") => TrustDecision::Trusted,
        Some("untrusted") => TrustDecision::Untrusted,
        _ => TrustDecision::Undecided,
    }
}

/// Core writer, split out so tests supply an explicit store path.
fn write_decision(store: &Path, dir: &Path, trusted: bool) -> Result<()> {
    let key = canonical_key(dir)
        .with_context(|| format!("cannot canonicalize project directory {}", dir.display()))?;
    let mut map = read_map(store);
    let value = if trusted { "trusted" } else { "untrusted" };
    map.insert(key, Value::String(value.to_string()));
    write_map_atomically(store, &map)
}

/// Canonical (symlink-resolved) key for `dir` as a lossy UTF-8 string. `None`
/// when the directory does not exist or cannot be resolved -- keying on a
/// non-canonical path would let a symlinked alias carry a separate decision.
fn canonical_key(dir: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(dir).ok()?;
    Some(canonical.to_string_lossy().into_owned())
}

/// Read the trust store as a flat `{ path: decision }` JSON object. A missing
/// file or any parse/shape error yields an empty map so reads fail closed.
fn read_map(store: &Path) -> Map<String, Value> {
    let Ok(contents) = std::fs::read_to_string(store) else {
        return Map::new();
    };
    match serde_json::from_str(&contents) {
        Ok(Value::Object(object)) => object,
        _ => Map::new(),
    }
}

/// Write the trust map via temp-file + fsync + rename so a crash never leaves a
/// half-written trust file (mirrors the settings writer in `config.rs`).
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

/// Trust store path: `IRIS_TRUST_PATH` override, else `~/.iris/trust.json`.
/// `None` when neither `IRIS_TRUST_PATH` nor `HOME` is set.
fn store_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("IRIS_TRUST_PATH") {
        return Some(PathBuf::from(path));
    }
    let home = env::var("HOME").ok().filter(|home| !home.is_empty())?;
    Some(Path::new(&home).join(".iris/trust.json"))
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
        let path = env::temp_dir().join(format!("iris-trust-test-{nanos}-{seq}"));
        fs::create_dir(&path).unwrap();
        TempDir { path }
    }

    #[test]
    fn unknown_project_reads_as_undecided() {
        let store = temp_dir();
        let project = temp_dir();
        assert_eq!(
            read_decision(&store.path.join("trust.json"), &project.path),
            TrustDecision::Undecided
        );
    }

    #[test]
    fn trusted_and_untrusted_round_trip() {
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        let project = temp_dir();

        write_decision(&store_file, &project.path, true).unwrap();
        assert_eq!(
            read_decision(&store_file, &project.path),
            TrustDecision::Trusted
        );

        // An untrusted decision is persisted too, and overwrites the prior one.
        write_decision(&store_file, &project.path, false).unwrap();
        assert_eq!(
            read_decision(&store_file, &project.path),
            TrustDecision::Untrusted
        );
    }

    #[test]
    fn decision_is_keyed_per_canonical_directory() {
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        let a = temp_dir();
        let b = temp_dir();
        write_decision(&store_file, &a.path, true).unwrap();
        // A different directory shares nothing with the trusted one.
        assert_eq!(
            read_decision(&store_file, &b.path),
            TrustDecision::Undecided
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_alias_resolves_to_the_same_decision() {
        use std::os::unix::fs::symlink;
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        let real = temp_dir();
        // Decide against the real (canonical) directory...
        write_decision(&store_file, &real.path, false).unwrap();

        // ...then look it up through a symlink that points at it. Canonicalizing
        // the alias resolves to the real dir, so the untrusted verdict holds and
        // the alias cannot dodge it.
        let link_parent = temp_dir();
        let alias = link_parent.path.join("alias");
        symlink(&real.path, &alias).unwrap();
        assert_eq!(read_decision(&store_file, &alias), TrustDecision::Untrusted);
    }

    #[test]
    fn malformed_store_reads_as_undecided() {
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        fs::write(&store_file, "{ not json").unwrap();
        let project = temp_dir();
        // A corrupt trust file fails closed (deny), never crashes startup.
        assert_eq!(
            read_decision(&store_file, &project.path),
            TrustDecision::Undecided
        );
    }

    #[test]
    fn write_preserves_other_projects() {
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        let a = temp_dir();
        let b = temp_dir();
        write_decision(&store_file, &a.path, true).unwrap();
        write_decision(&store_file, &b.path, false).unwrap();
        // Both decisions coexist in one store.
        assert_eq!(read_decision(&store_file, &a.path), TrustDecision::Trusted);
        assert_eq!(
            read_decision(&store_file, &b.path),
            TrustDecision::Untrusted
        );
    }
}
