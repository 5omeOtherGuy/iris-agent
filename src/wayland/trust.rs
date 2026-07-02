//! Per-cwd project permission policy store (Tier 2, ADR-0027).
//!
//! Repurposes the former project-trust store (ADR-0026 removed its
//! fragment-gating role): the same persistence substrate -- `~/.iris/trust.json`
//! keyed by the **canonical** (symlink-resolved) directory, HOME-owned, atomic
//! writes, fail-closed reads, `IRIS_TRUST_PATH` override -- now carries a
//! per-project permission policy instead of a tri-state trust value:
//!
//! - `allow_tools`: non-bash tools (`write`/`edit`) whose calls auto-approve.
//! - `allow_bash`: exact `bash` command strings that auto-approve.
//! - `allow_bash_prefix`: `bash` command prefixes that auto-approve.
//! - `sandbox`: per-project sandbox posture (stored; enforcement deferred).
//!
//! Security posture (the ADR-0027 invariants):
//! - The store is HOME-owned and canonical-cwd-keyed -- NEVER a repo-committed
//!   file. A cloned repo cannot ship a config that pre-approves its own tools;
//!   nothing under the workspace is ever read here (invariant 1).
//! - This module is data the Nexus approval gate reads (ADR-0005); enforcement
//!   -- including the unconditional destructive re-prompt (invariant 2) --
//!   lives in `nexus.rs`, never here.
//! - Grants are written only on deliberate user action (`[p]` at an approval
//!   prompt, or the `/trust` editor); nothing self-waives (invariant 4). A
//!   grant write never touches the stored sandbox posture (invariant 3).
//! - Reads fail closed: a missing, malformed, or legacy-shaped entry (the old
//!   `"trusted"`/`"untrusted"` strings) yields the empty policy, which grants
//!   nothing.

use std::collections::BTreeSet;
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::nexus::{PolicyGrant, ProjectPolicy, ProjectPolicySink};

/// The stored per-project policy record. `#[serde(default)]` keeps every field
/// optional on disk so a partial record still parses; unknown values fail
/// closed field-by-field via the whole-record parse (a malformed record reads
/// as empty).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ProjectPolicyRecord {
    /// Non-bash tools (`write`/`edit`) whose calls auto-approve.
    pub(crate) allow_tools: BTreeSet<String>,
    /// Exact `bash` command strings that auto-approve.
    pub(crate) allow_bash: BTreeSet<String>,
    /// `bash` command prefixes that auto-approve (token-boundary matched by the
    /// enforcement layer).
    pub(crate) allow_bash_prefix: BTreeSet<String>,
    /// Per-project sandbox posture. Stored and round-tripped only: enforcement
    /// is deferred, and no grant/revoke path writes it (loosening the sandbox
    /// is an explicit user action, never automatic -- invariant 3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sandbox: Option<String>,
}

impl ProjectPolicyRecord {
    /// Convert to the enforcement-layer policy data consumed by Nexus. The
    /// sandbox posture is intentionally not part of the approval policy.
    pub(crate) fn to_policy(&self) -> ProjectPolicy {
        ProjectPolicy {
            tools: self.allow_tools.clone(),
            bash_exact: self.allow_bash.clone(),
            bash_prefix: self.allow_bash_prefix.clone(),
        }
    }

    /// Whether the record carries no grants and no sandbox posture.
    pub(crate) fn is_empty(&self) -> bool {
        self.allow_tools.is_empty()
            && self.allow_bash.is_empty()
            && self.allow_bash_prefix.is_empty()
            && self.sandbox.is_none()
    }

    fn apply_grant(&mut self, grant: &PolicyGrant) {
        match grant {
            PolicyGrant::Tool(name) => {
                self.allow_tools.insert(name.clone());
            }
            PolicyGrant::BashExact(command) => {
                self.allow_bash.insert(command.clone());
            }
        }
    }
}

/// The recorded policy for `dir`, or the empty policy when none is stored, the
/// store path / directory cannot be resolved, or the entry is malformed.
/// Fail-closed: an empty policy grants nothing. Only the HOME-owned store is
/// ever consulted -- never any file under `dir` (invariant 1).
pub(crate) fn policy_for(dir: &Path) -> ProjectPolicyRecord {
    let Some(store) = store_path() else {
        return ProjectPolicyRecord::default();
    };
    read_record(&store, dir)
}

/// Persist a single grant for `dir` (deliberate user action at an approval
/// prompt). Read-modify-write: other projects' entries and this project's
/// sandbox posture are preserved untouched.
pub(crate) fn apply_grant(dir: &Path, grant: &PolicyGrant) -> Result<()> {
    let store = resolve_store()?;
    let mut record = read_record(&store, dir);
    record.apply_grant(grant);
    write_record(&store, dir, &record)
}

/// Replace the whole stored policy record for `dir` (the `/trust` editor's
/// grant/revoke path). An empty record removes the entry.
pub(crate) fn set_policy(dir: &Path, record: &ProjectPolicyRecord) -> Result<()> {
    let store = resolve_store()?;
    write_record(&store, dir, record)
}

/// Tier-2 persistence sink handed to the Nexus agent: persists a grant for the
/// session's cwd when the user chooses "always for this project".
pub(crate) struct PolicyStoreSink {
    cwd: PathBuf,
}

impl PolicyStoreSink {
    pub(crate) fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

impl ProjectPolicySink for PolicyStoreSink {
    fn persist(&self, grant: &PolicyGrant) -> Result<()> {
        apply_grant(&self.cwd, grant)
    }
}

fn resolve_store() -> Result<PathBuf> {
    store_path().context("cannot resolve the policy store path (set HOME or IRIS_TRUST_PATH)")
}

/// Core reader, split out so tests supply an explicit store path. A missing
/// store, an unresolvable directory, or a malformed/legacy entry (the old
/// tri-state `"trusted"` strings) all read as the empty policy (fail closed).
fn read_record(store: &Path, dir: &Path) -> ProjectPolicyRecord {
    let Some(key) = canonical_key(dir) else {
        return ProjectPolicyRecord::default();
    };
    let map = read_map(store);
    match map.get(&key) {
        Some(value) => serde_json::from_value(value.clone()).unwrap_or_default(),
        None => ProjectPolicyRecord::default(),
    }
}

/// Core writer, split out so tests supply an explicit store path. An empty
/// record removes the project's entry instead of storing an empty object.
fn write_record(store: &Path, dir: &Path, record: &ProjectPolicyRecord) -> Result<()> {
    let key = canonical_key(dir)
        .with_context(|| format!("cannot canonicalize project directory {}", dir.display()))?;
    let mut map = read_map(store);
    if record.is_empty() {
        map.remove(&key);
    } else {
        map.insert(key, serde_json::to_value(record)?);
    }
    write_map_atomically(store, &map)
}

/// Canonical (symlink-resolved) key for `dir` as a lossy UTF-8 string. `None`
/// when the directory does not exist or cannot be resolved -- keying on a
/// non-canonical path would let a symlinked alias carry a separate policy.
fn canonical_key(dir: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(dir).ok()?;
    Some(canonical.to_string_lossy().into_owned())
}

/// Read the store as a flat `{ path: record }` JSON object. A missing file or
/// any parse/shape error yields an empty map so reads fail closed.
fn read_map(store: &Path) -> Map<String, Value> {
    let Ok(contents) = std::fs::read_to_string(store) else {
        return Map::new();
    };
    match serde_json::from_str(&contents) {
        Ok(Value::Object(object)) => object,
        _ => Map::new(),
    }
}

/// Write the policy map via temp-file + fsync + rename so a crash never leaves
/// a half-written store (mirrors the settings writer in `config.rs`).
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

/// Policy store path: `IRIS_TRUST_PATH` override, else `~/.iris/trust.json`.
/// `None` when neither `IRIS_TRUST_PATH` nor `HOME` is set. NEVER derived from
/// the workspace: a repo-committed file can never become the store.
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

    fn grant_write() -> PolicyGrant {
        PolicyGrant::Tool("write".to_string())
    }

    #[test]
    fn unknown_project_reads_as_the_empty_policy() {
        let store = temp_dir();
        let project = temp_dir();
        let record = read_record(&store.path.join("trust.json"), &project.path);
        assert!(record.is_empty());
        assert!(record.to_policy().tools.is_empty());
    }

    #[test]
    fn grants_round_trip_and_persist_across_reads() {
        // A project grant persists across a fresh read of the store -- the
        // cross-session persistence #209 asks for (unlike session_allowed).
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        let project = temp_dir();

        let mut record = read_record(&store_file, &project.path);
        record.apply_grant(&grant_write());
        record.apply_grant(&PolicyGrant::BashExact("cargo test".to_string()));
        write_record(&store_file, &project.path, &record).unwrap();

        // A fresh read (a "new session") sees the same grants.
        let reread = read_record(&store_file, &project.path);
        assert!(reread.allow_tools.contains("write"));
        assert!(reread.allow_bash.contains("cargo test"));
        let policy = reread.to_policy();
        assert!(policy.tools.contains("write"));
        assert!(policy.bash_exact.contains("cargo test"));
    }

    #[test]
    fn policy_is_keyed_per_canonical_directory() {
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        let a = temp_dir();
        let b = temp_dir();
        let mut record = ProjectPolicyRecord::default();
        record.apply_grant(&grant_write());
        write_record(&store_file, &a.path, &record).unwrap();
        // A different directory shares nothing with the granted one (per-cwd,
        // not per-git-root: sibling directories do not share policy).
        assert!(read_record(&store_file, &b.path).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_alias_resolves_to_the_same_policy() {
        use std::os::unix::fs::symlink;
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        let real = temp_dir();
        let mut record = ProjectPolicyRecord::default();
        record.apply_grant(&grant_write());
        write_record(&store_file, &real.path, &record).unwrap();

        // Looking the policy up through a symlinked alias resolves to the real
        // dir, so the same policy applies (and an alias cannot carry its own).
        let link_parent = temp_dir();
        let alias = link_parent.path.join("alias");
        symlink(&real.path, &alias).unwrap();
        assert!(
            read_record(&store_file, &alias)
                .allow_tools
                .contains("write")
        );
    }

    #[test]
    fn malformed_store_reads_as_the_empty_policy() {
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        fs::write(&store_file, "{ not json").unwrap();
        let project = temp_dir();
        // A corrupt store fails closed (grants nothing), never crashes startup.
        assert!(read_record(&store_file, &project.path).is_empty());
    }

    #[test]
    fn legacy_tri_state_entry_reads_as_the_empty_policy() {
        // Pre-ADR-0027 stores carried `{ "<dir>": "trusted" }`. The legacy
        // string value must not parse into any grant: it fails closed.
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        let project = temp_dir();
        let key = canonical_key(&project.path).unwrap();
        fs::write(&store_file, format!("{{ \"{key}\": \"trusted\" }}")).unwrap();
        assert!(read_record(&store_file, &project.path).is_empty());
    }

    #[test]
    fn write_preserves_other_projects() {
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        let a = temp_dir();
        let b = temp_dir();
        let mut record_a = ProjectPolicyRecord::default();
        record_a.apply_grant(&grant_write());
        write_record(&store_file, &a.path, &record_a).unwrap();
        let mut record_b = ProjectPolicyRecord::default();
        record_b.apply_grant(&PolicyGrant::BashExact("ls".to_string()));
        write_record(&store_file, &b.path, &record_b).unwrap();
        // Both policies coexist in one store.
        assert!(
            read_record(&store_file, &a.path)
                .allow_tools
                .contains("write")
        );
        assert!(read_record(&store_file, &b.path).allow_bash.contains("ls"));
    }

    #[test]
    fn revoking_every_grant_removes_the_entry() {
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        let project = temp_dir();
        let mut record = ProjectPolicyRecord::default();
        record.apply_grant(&grant_write());
        write_record(&store_file, &project.path, &record).unwrap();

        // The /trust editor writes back an emptied record: the entry vanishes.
        write_record(&store_file, &project.path, &ProjectPolicyRecord::default()).unwrap();
        let map = read_map(&store_file);
        assert!(map.is_empty(), "an empty record must remove the entry");
    }

    // ---- ADR-0027 invariant 1: the store is HOME-owned, never repo-committed --

    #[test]
    fn invariant_1_a_repo_shipped_policy_file_grants_nothing() {
        // A cloned repo ships `.iris/trust.json` (and a root-level trust.json)
        // pre-approving its own tools. Neither is ever consulted: the reader
        // only sees the HOME-owned store it is given, and `store_path()` never
        // derives from the workspace. The workspace policy stays empty.
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        let ws = temp_dir();
        let key = canonical_key(&ws.path).unwrap();
        let hostile = format!(
            "{{ \"{key}\": {{ \"allow_tools\": [\"write\", \"edit\"], \"allow_bash_prefix\": [\"\"] }} }}"
        );
        fs::create_dir_all(ws.path.join(".iris")).unwrap();
        fs::write(ws.path.join(".iris/trust.json"), &hostile).unwrap();
        fs::write(ws.path.join("trust.json"), &hostile).unwrap();

        let record = read_record(&store_file, &ws.path);
        assert!(
            record.is_empty(),
            "a repo-shipped policy file must never grant: {record:?}"
        );
        assert!(record.to_policy().tools.is_empty());
    }

    // ---- ADR-0027 invariant 3: grants never touch the sandbox posture ---------

    #[test]
    fn invariant_3_grant_writes_preserve_and_never_create_sandbox_posture() {
        let store = temp_dir();
        let store_file = store.path.join("trust.json");
        let project = temp_dir();

        // A grant on a fresh project must not create a sandbox posture.
        let mut record = read_record(&store_file, &project.path);
        record.apply_grant(&grant_write());
        write_record(&store_file, &project.path, &record).unwrap();
        assert_eq!(read_record(&store_file, &project.path).sandbox, None);

        // A pre-existing posture (user-set) survives a later grant untouched:
        // the read-modify-write grant path can never loosen (or change) it.
        let key = canonical_key(&project.path).unwrap();
        fs::write(
            &store_file,
            format!(
                "{{ \"{key}\": {{ \"allow_tools\": [\"write\"], \"sandbox\": \"restricted\" }} }}"
            ),
        )
        .unwrap();
        let mut record = read_record(&store_file, &project.path);
        record.apply_grant(&PolicyGrant::BashExact("cargo test".to_string()));
        write_record(&store_file, &project.path, &record).unwrap();
        let reread = read_record(&store_file, &project.path);
        assert_eq!(reread.sandbox.as_deref(), Some("restricted"));
        assert!(reread.allow_bash.contains("cargo test"));
    }
}
