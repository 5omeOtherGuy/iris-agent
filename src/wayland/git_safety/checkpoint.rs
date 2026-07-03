//! Task checkpoint chain (issue #263, ADR-0028).
//!
//! A checkpoint is a real git commit object anchored by a ref in the hidden
//! `refs/iris/checkpoints/<task-id>/` namespace, built entirely with git
//! plumbing against a *temporary* index (`GIT_INDEX_FILE`) so the user's index,
//! `HEAD`, stash, and visible refs are never touched. The chain is the op-log:
//! each mutating step appends a checkpoint whose tree snapshots the current
//! content of every ledger path, parented to the previous checkpoint. A `base`
//! ref (seq 0) holds the pre-task content of those same paths, so a rollback to
//! base restores the exact pre-task state and a rollback to an intermediate
//! checkpoint restores that step -- both by materializing ledger paths from a
//! git tree, which gives create/edit/delete/rename/mode/binary the correct
//! restore semantics for free (ADR-0028).
//!
//! Nothing here reads `HEAD`, the working index, or any ref outside the task's
//! own namespace; GC on settlement is likewise scoped to that namespace only.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use super::git::{git_env_stdout, git_io, git_stdout};

/// The hidden namespace prefix all checkpoint refs live under. GC and listing
/// are scoped to `<PREFIX>/<task-id>/` so foreign refs (branches, tags, another
/// task's checkpoints) are never enumerated, moved, or deleted.
const PREFIX: &str = "refs/iris/checkpoints";

/// The base (pre-task) checkpoint's sequence number. Every intermediate
/// checkpoint is seq >= 1; a rollback to seq 0 restores the pre-task state.
const BASE_SEQ: u64 = 0;

/// A file's git mode. Only the modes git tracks for a blob; a directory is never
/// a ledger path. Symlinks degrade to [`Mode::Normal`] (content read through the
/// link), an accepted edge for the rare symlink-in-ledger case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Normal,
    Exec,
}

impl Mode {
    /// The 6-digit octal git tree/index mode string.
    fn as_octal(self) -> &'static str {
        match self {
            Mode::Normal => "100644",
            Mode::Exec => "100755",
        }
    }

    /// Detect a path's mode from its filesystem metadata (executable bit).
    /// Absent/unreadable falls back to [`Mode::Normal`].
    pub(super) fn of(path: &Path) -> Mode {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match std::fs::metadata(path) {
                Ok(meta) if meta.permissions().mode() & 0o111 != 0 => Mode::Exec,
                _ => Mode::Normal,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Mode::Normal
        }
    }

    fn from_octal(octal: &[u8]) -> Mode {
        if octal == b"100755" {
            Mode::Exec
        } else {
            Mode::Normal
        }
    }
}

/// A content blob already written to the object store: its sha and file mode.
#[derive(Debug, Clone)]
struct Blob {
    sha: String,
    mode: Mode,
}

/// One restore point the user can roll back to. `seq` names the ref, `commit` is
/// its tip, and `label` is the human op-log description the rollback UI lists.
/// The `turn`/`tool_call`/`timestamp` op-log metadata is recorded now and
/// consumed by the final-diff view (#264); it is a seam field until then.
#[derive(Debug, Clone)]
pub(super) struct RestorePoint {
    pub(super) seq: u64,
    pub(super) commit: String,
    #[allow(dead_code)]
    pub(super) turn: u64,
    #[allow(dead_code)]
    pub(super) tool_call: Option<String>,
    #[allow(dead_code)]
    pub(super) timestamp: SystemTime,
    pub(super) label: String,
}

/// The task's git-backed checkpoint chain. Holds the growing pre-task (`before`)
/// snapshot and the ordered restore points; the durable state is the refs in the
/// task namespace, so this is a thin coordinator over git plumbing.
pub(super) struct CheckpointChain {
    workspace: PathBuf,
    task_id: String,
    /// Pre-task blob for every ledger path (first-touch wins). `None` = the path
    /// did not exist before the task (a create), so a base rollback deletes it.
    before: BTreeMap<PathBuf, Option<Blob>>,
    /// Ordered intermediate restore points (seq >= 1), oldest first.
    points: Vec<RestorePoint>,
    /// Next intermediate sequence number.
    next_seq: u64,
}

impl CheckpointChain {
    pub(super) fn new(workspace: PathBuf, task_id: String) -> Self {
        Self {
            workspace,
            task_id,
            before: BTreeMap::new(),
            points: Vec::new(),
            next_seq: BASE_SEQ + 1,
        }
    }

    /// Rehydrate a chain for an unsettled task from its durable git refs on
    /// resume (ADR-0028 crash recovery), so a post-restart `/rollback`,
    /// `/accept`, or `/checkpoint` operates on the real checkpoint chain instead
    /// of reporting "no unsettled Iris changes". `ledger_paths` is the full set
    /// of ledger paths (absolute) recovered from the persisted record -- it is
    /// the authoritative key set because a create (`before = None`) is absent
    /// from the base tree yet is still a ledger path that a base rollback must
    /// delete. The base (seq 0) tree supplies each path's pre-task blob; a path
    /// missing from it is a create. Intermediate refs (seq >= 1) become restore
    /// points with generic op-log labels (the original turn/tool metadata lived
    /// only in memory; the commit trailer keeps the durable copy for #264).
    pub(super) fn load(
        workspace: PathBuf,
        task_id: String,
        ledger_paths: &[PathBuf],
    ) -> Result<Self> {
        let mut chain = Self::new(workspace, task_id);
        let refs = list_task_refs(&chain.workspace, &chain.task_id)?;
        let base_tree = match refs.get(&BASE_SEQ) {
            Some(commit) => chain.tree_entries(commit)?,
            None => BTreeMap::new(),
        };
        for path in ledger_paths {
            let rel = chain.rel_bytes(path)?;
            let blob = base_tree.get(&rel).cloned();
            chain.before.insert(path.clone(), blob);
        }
        let mut seqs: Vec<u64> = refs.keys().copied().filter(|&s| s != BASE_SEQ).collect();
        seqs.sort_unstable();
        for seq in &seqs {
            let commit = refs.get(seq).cloned().unwrap_or_default();
            chain.points.push(RestorePoint {
                seq: *seq,
                commit,
                turn: 0,
                tool_call: None,
                timestamp: SystemTime::now(),
                label: "recovered checkpoint".to_string(),
            });
        }
        chain.next_seq = seqs.last().copied().unwrap_or(BASE_SEQ) + 1;
        Ok(chain)
    }

    /// Restore points offered by the rollback UI: the pre-task base first, then
    /// each intermediate checkpoint oldest-to-newest.
    pub(super) fn restore_points(&self) -> Vec<RestorePoint> {
        self.points.clone()
    }

    /// Ledger paths whose current on-disk bytes diverged from the chain tip --
    /// Iris's last recorded (expected-after) state. A non-empty result means a
    /// user edit (or deletion, or re-creation) landed on a ledger path after
    /// Iris's last write: ADR-0028's TOCTOU rule attributes the ambiguous bytes
    /// to the user, so rollback must NOT clobber them. Byte-exact comparison
    /// against the tip tree (the last full checkpoint of every ledger path).
    pub(super) fn diverged_from_tip(&self) -> Result<Vec<PathBuf>> {
        let Some(tip) = self.tip_commit()? else {
            return Ok(Vec::new());
        };
        let tree = self.tree_entries(&tip)?;
        let mut diverged = Vec::new();
        for path in self.before.keys() {
            let rel = self.rel_bytes(path)?;
            let expected = match tree.get(&rel) {
                Some(blob) => Some(self.read_blob(&blob.sha)?),
                None => None,
            };
            let current = std::fs::read(path).ok();
            if current != expected {
                diverged.push(path.clone());
            }
        }
        Ok(diverged)
    }

    /// The chain tip commit: the highest intermediate ref (seq >= 1), or the base
    /// (seq 0) when no intermediate exists. Resolved from the durable refs, not
    /// the in-memory points, so a recovery append (which advances refs without
    /// touching `points`) is still seen as the tip.
    fn tip_commit(&self) -> Result<Option<String>> {
        let refs = list_task_refs(&self.workspace, &self.task_id)?;
        let tip = refs
            .iter()
            .filter(|(seq, _)| **seq != BASE_SEQ)
            .max_by_key(|(seq, _)| **seq)
            .map(|(_, commit)| commit.clone())
            .or_else(|| refs.get(&BASE_SEQ).cloned());
        Ok(tip)
    }

    /// Number of intermediate checkpoints recorded (test/GC accounting).
    pub(super) fn len(&self) -> usize {
        self.points.len()
    }

    /// Number of distinct ledger paths tracked by the chain. The rollback
    /// summary uses it as an Iris-change count for a rehydrated task, whose
    /// in-memory ledger is empty but whose paths survive in the chain.
    pub(super) fn ledger_path_count(&self) -> usize {
        self.before.len()
    }

    /// Net-diff inputs (issue #264): for every ledger path, its pre-task bytes
    /// (read back from the base snapshot blob; `None` = a create) and its current
    /// bytes. `root` is the source tree the current side comes from -- normally
    /// the workspace, but a parameter so a later worktree-apply review
    /// (#267/#271) can diff the same ledger against a different tree. Only ledger
    /// paths are visited, so a user-modified file never appears.
    ///
    /// TOCTOU exclusion (issue #264 finding 1, ADR-0028): when diffing the live
    /// workspace, each path's current disk bytes are compared against Iris's last
    /// recorded write (the checkpoint-chain tip, the same tip [`diverged_from_tip`]
    /// reconciles rollback against). If they differ, the user edited the path
    /// after Iris's last touch; those bytes are user-attributed, so the current
    /// side becomes the tip blob (Iris's last recorded state) -- never the user's
    /// bytes rendered as Iris output -- and the path is flagged diverged so the
    /// diff carries an explicit notice. Divergence detection is skipped for an
    /// alternate `root` (the worktree-apply seam), where the current side is that
    /// tree by definition and there is no "live disk vs tip" relationship.
    pub(super) fn net_diff_inputs(&self, root: &Path) -> Result<Vec<super::net_diff::NetPath>> {
        // Iris's last recorded snapshot of every ledger path, only when diffing
        // the live workspace (see divergence note above).
        let tip_tree = if root == self.workspace {
            match self.tip_commit()? {
                Some(tip) => Some(self.tree_entries(&tip)?),
                None => None,
            }
        } else {
            None
        };
        let mut out = Vec::new();
        for (path, blob) in &self.before {
            let rel = path
                .strip_prefix(&self.workspace)
                .unwrap_or(path)
                .to_path_buf();
            let pre = match blob {
                Some(blob) => Some(self.read_blob(&blob.sha)?),
                None => None,
            };
            let disk = std::fs::read(root.join(&rel)).ok();
            let (cur, diverged) = match &tip_tree {
                Some(tree) => {
                    let rel_key = self.rel_bytes(path)?;
                    let tip = match tree.get(&rel_key) {
                        Some(blob) => Some(self.read_blob(&blob.sha)?),
                        None => None,
                    };
                    if disk == tip {
                        (disk, false)
                    } else {
                        // User bytes landed after Iris's last write: show Iris's
                        // last recorded state, flag the divergence.
                        (tip, true)
                    }
                }
                None => (disk, false),
            };
            out.push(super::net_diff::NetPath {
                rel,
                pre,
                cur,
                diverged,
            });
        }
        Ok(out)
    }

    /// Record the pre-task content of a ledger path the first time it is seen.
    /// Idempotent: a later touch of the same path never overwrites the captured
    /// pre-task bytes. `bytes` is `None` for a path that did not exist pre-task.
    pub(super) fn note_before(
        &mut self,
        path: &Path,
        bytes: Option<(Vec<u8>, Mode)>,
    ) -> Result<()> {
        if self.before.contains_key(path) {
            return Ok(());
        }
        let blob = match bytes {
            Some((bytes, mode)) => Some(self.write_blob(path, &bytes, mode)?),
            None => None,
        };
        self.before.insert(path.to_path_buf(), blob);
        Ok(())
    }

    /// Append a checkpoint: snapshot the current on-disk content of every noted
    /// ledger path into a tree, commit it parented to the previous checkpoint,
    /// and move the task's tip ref. Also (re)writes the `base` ref from the
    /// accumulated pre-task snapshot so a base rollback always reflects every
    /// path touched so far. Returns the new restore point.
    pub(super) fn checkpoint(
        &mut self,
        turn: u64,
        tool_call: Option<String>,
        label: String,
    ) -> Result<RestorePoint> {
        // Rewrite base from the pre-task snapshot (grows as paths are noted).
        let base_entries = self
            .before
            .iter()
            .filter_map(|(path, blob)| blob.as_ref().map(|blob| (path.clone(), blob.clone())));
        let base_tree = self.build_tree(base_entries)?;
        let base_commit = self.commit_tree(&base_tree, None, "iris-checkpoint base")?;
        self.update_ref(BASE_SEQ, &base_commit)?;

        // Snapshot the current disk state of every noted path into the tip tree.
        let mut current = Vec::new();
        for path in self.before.keys() {
            if let Some(blob) = self.read_current(path)? {
                current.push((path.clone(), blob));
            }
        }
        let tree = self.build_tree(current.into_iter())?;
        let parent = self
            .points
            .last()
            .map(|p| p.commit.clone())
            .unwrap_or(base_commit);
        let message = commit_message(turn, tool_call.as_deref(), &label);
        let commit = self.commit_tree(&tree, Some(&parent), &message)?;
        let seq = self.next_seq;
        self.next_seq += 1;
        self.update_ref(seq, &commit)?;
        let point = RestorePoint {
            seq,
            commit,
            turn,
            tool_call,
            timestamp: SystemTime::now(),
            label,
        };
        self.points.push(point.clone());
        Ok(point)
    }

    /// Materialize every ledger path from the checkpoint tree at `seq`, EXCEPT
    /// paths in `excluded` -- the user-diverged paths whose current disk bytes
    /// rollback must preserve (ADR-0028 TOCTOU rule). A path present in the tree
    /// (and not excluded) is written back byte-for-byte with its recorded mode; a
    /// ledger path absent from the tree is deleted (undoing a create); an
    /// excluded path is left exactly as it is on disk, neither rewritten nor
    /// deleted. Only ledger paths are touched -- user paths never appear in
    /// `before`.
    pub(super) fn rollback_to_excluding(
        &self,
        seq: u64,
        excluded: &BTreeSet<PathBuf>,
    ) -> Result<()> {
        let commit = self
            .resolve_ref(seq)?
            .with_context(|| format!("checkpoint {seq} for task {} is missing", self.task_id))?;
        let tree = self.tree_entries(&commit)?;
        for path in self.before.keys() {
            if excluded.contains(path) {
                continue;
            }
            let rel = self.rel_bytes(path)?;
            match tree.get(&rel) {
                Some(blob) => {
                    let bytes = self.read_blob(&blob.sha)?;
                    write_file(path, &bytes, blob.mode)?;
                }
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        Ok(())
    }

    /// GC the task's intermediate checkpoints, keeping the newest `keep` (the
    /// base ref is always kept). Deletes refs *only* under this task's namespace;
    /// foreign refs are never enumerated. Called at settlement.
    pub(super) fn gc(&mut self, keep: usize) -> Result<()> {
        let refs = list_task_refs(&self.workspace, &self.task_id)?;
        // Intermediate seqs present in git, sorted ascending.
        let mut seqs: Vec<u64> = refs.keys().copied().filter(|&s| s != BASE_SEQ).collect();
        seqs.sort_unstable();
        let drop_count = seqs.len().saturating_sub(keep);
        for &seq in seqs.iter().take(drop_count) {
            self.delete_ref(seq)?;
        }
        self.points = self
            .points
            .iter()
            .filter(|p| !seqs.iter().take(drop_count).any(|&s| s == p.seq))
            .cloned()
            .collect();
        Ok(())
    }

    /// Delete every ref in the task namespace (full settlement teardown, e.g.
    /// accept). Scoped to `<PREFIX>/<task-id>/` only.
    pub(super) fn destroy(&mut self) -> Result<()> {
        let refs = list_task_refs(&self.workspace, &self.task_id)?;
        for &seq in refs.keys() {
            self.delete_ref(seq)?;
        }
        self.points.clear();
        Ok(())
    }

    // --- git plumbing (temporary index / object store only) --------------

    /// Write `bytes` as a blob (`hash-object -w --stdin --no-filters`) and
    /// return it with `mode`. `--no-filters` stores exact bytes so a clean/smudge
    /// gitattribute can never mangle the restored content.
    fn write_blob(&self, _path: &Path, bytes: &[u8], mode: Mode) -> Result<Blob> {
        let out = git_io(
            &self.workspace,
            &["hash-object", "-w", "--stdin", "--no-filters"],
            &[],
            bytes,
        )?;
        let sha = String::from_utf8_lossy(&out).trim().to_string();
        if sha.is_empty() {
            bail!("git hash-object produced no sha");
        }
        Ok(Blob { sha, mode })
    }

    /// Build a tree from `entries` via a throwaway index (`GIT_INDEX_FILE`), so
    /// the user's real index is never read or written. Feeds
    /// `update-index --index-info` on stdin (`<mode> SP <sha>TAB<relpath>`, raw
    /// bytes for non-UTF-8 paths) then `write-tree`.
    fn build_tree(&self, entries: impl Iterator<Item = (PathBuf, Blob)>) -> Result<String> {
        let mut raw = Vec::new();
        for (path, blob) in entries {
            raw.push((self.rel_bytes(&path)?, blob));
        }
        self.build_tree_raw(raw.into_iter())
    }

    /// Build a tree from already-workspace-relative path bytes. The lower-level
    /// half of [`build_tree`]: recovery snapshots overlay diverged blobs onto a
    /// previous checkpoint's tree entries, which are already `relpath` bytes, so
    /// they skip the [`PathBuf`] round-trip.
    fn build_tree_raw(&self, entries: impl Iterator<Item = (Vec<u8>, Blob)>) -> Result<String> {
        let index = TempIndex::new(&self.task_id);
        let index_env: &OsStr = index.path.as_os_str();
        let env = [("GIT_INDEX_FILE", index_env)];

        let mut stdin: Vec<u8> = Vec::new();
        for (rel, blob) in entries {
            stdin.extend_from_slice(blob.mode.as_octal().as_bytes());
            stdin.push(b' ');
            stdin.extend_from_slice(blob.sha.as_bytes());
            stdin.push(b'\t');
            stdin.extend_from_slice(&rel);
            stdin.push(b'\n');
        }
        // An empty tree still needs a write-tree; update-index with empty stdin
        // is a no-op against the fresh (empty) temporary index.
        git_io(
            &self.workspace,
            &["update-index", "--index-info"],
            &env,
            &stdin,
        )?;
        let out = git_env_stdout(&self.workspace, &["write-tree"], &env)?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// `commit-tree` with a fixed, non-interactive identity so the commit is
    /// deterministic and never picks up the user's `user.name`/`user.email`
    /// prompts. Author/committer dates come from git's clock.
    fn commit_tree(&self, tree: &str, parent: Option<&str>, message: &str) -> Result<String> {
        let mut args = vec!["commit-tree", tree];
        if let Some(parent) = parent {
            args.push("-p");
            args.push(parent);
        }
        let env: [(&str, &OsStr); 4] = [
            ("GIT_AUTHOR_NAME", OsStr::new("iris")),
            ("GIT_AUTHOR_EMAIL", OsStr::new("iris@localhost")),
            ("GIT_COMMITTER_NAME", OsStr::new("iris")),
            ("GIT_COMMITTER_EMAIL", OsStr::new("iris@localhost")),
        ];
        let out = git_io(&self.workspace, &args, &env, message.as_bytes())?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    fn ref_name(&self, seq: u64) -> String {
        format!("{PREFIX}/{}/{seq:010}", self.task_id)
    }

    fn update_ref(&self, seq: u64, commit: &str) -> Result<()> {
        let name = self.ref_name(seq);
        git_stdout(&self.workspace, &["update-ref", &name, commit])?;
        Ok(())
    }

    fn delete_ref(&self, seq: u64) -> Result<()> {
        let name = self.ref_name(seq);
        // `-d` with no old-value is fine: the ref is ours and we hold no lock.
        git_stdout(&self.workspace, &["update-ref", "-d", &name])?;
        Ok(())
    }

    fn resolve_ref(&self, seq: u64) -> Result<Option<String>> {
        let refs = list_task_refs(&self.workspace, &self.task_id)?;
        Ok(refs.get(&seq).cloned())
    }

    /// Read a commit's tree entries (recursive) into a `relpath -> Blob` map via
    /// `ls-tree -r -z`. Bytes throughout so non-UTF-8 paths round-trip.
    fn tree_entries(&self, commit: &str) -> Result<BTreeMap<Vec<u8>, Blob>> {
        let out = git_stdout(&self.workspace, &["ls-tree", "-r", "-z", commit])?;
        let mut map = BTreeMap::new();
        for record in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
            // "<mode> SP <type> SP <sha>TAB<path>"
            let Some(tab) = record.iter().position(|&b| b == b'\t') else {
                continue;
            };
            let (meta, path) = record.split_at(tab);
            let path = &path[1..]; // drop the TAB
            let fields: Vec<&[u8]> = meta.splitn(3, |&b| b == b' ').collect();
            if fields.len() != 3 {
                continue;
            }
            let mode = Mode::from_octal(fields[0]);
            let sha = String::from_utf8_lossy(fields[2]).trim().to_string();
            map.insert(path.to_vec(), Blob { sha, mode });
        }
        Ok(map)
    }

    fn read_blob(&self, sha: &str) -> Result<Vec<u8>> {
        git_stdout(&self.workspace, &["cat-file", "blob", sha])
    }

    /// Current on-disk content of a ledger path as a freshly-written blob, or
    /// `None` when the path is absent (a delete).
    fn read_current(&self, path: &Path) -> Result<Option<Blob>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(self.write_blob(path, &bytes, Mode::of(path))?)),
            Err(_) => Ok(None),
        }
    }

    /// Workspace-relative path bytes for a normalized absolute ledger path.
    fn rel_bytes(&self, path: &Path) -> Result<Vec<u8>> {
        let rel = path.strip_prefix(&self.workspace).with_context(|| {
            format!(
                "ledger path {} escapes the workspace {}",
                path.display(),
                self.workspace.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Ok(rel.as_os_str().as_bytes().to_vec())
        }
        #[cfg(not(unix))]
        {
            Ok(rel.to_string_lossy().into_owned().into_bytes())
        }
    }
}

/// Resolve a path's committed (index, then `HEAD`) blob content as its pre-task
/// image, for a bash-attributed change to a previously-clean file whose
/// pre-mutation bytes were not snapshotted. `None` when the path has no tracked
/// predecessor (a create) or its name is non-UTF-8 (rev-spec args are text, so
/// that rare case degrades to "create" -- base rollback then deletes it). The
/// mode is read from the current file (best-effort).
pub(super) fn committed_blob(workspace: &Path, path: &Path) -> Option<(Vec<u8>, Mode)> {
    let rel = path.strip_prefix(workspace).ok()?.to_str()?;
    for spec in [format!(":{rel}"), format!("HEAD:{rel}")] {
        if let Ok(bytes) = git_stdout(workspace, &["cat-file", "blob", &spec]) {
            return Some((bytes, Mode::of(path)));
        }
    }
    None
}

/// Append a recovery checkpoint to an existing chain WITHOUT a live in-memory
/// [`CheckpointChain`] (crash recovery on resume, ADR-0028): snapshot the FULL
/// current on-disk state of every ledger path into a tree, commit it parented to
/// the chain's current tip, and advance the tip ref. The `base` ref is left
/// untouched so the pre-task snapshot is preserved. No-op when the task has no
/// existing refs (nothing to recover).
///
/// The recovery tree is a *complete* snapshot, not just the diverged paths: it
/// starts from the current tip's tree (the last full checkpoint of every ledger
/// path) and overlays each diverged path's current disk bytes (or removes it
/// when it now vanished). A recovery tree built from only the diverged paths
/// would be a delete-bomb -- [`CheckpointChain::rollback_to_excluding`] treats any ledger
/// path absent from the tree as a delete, so rolling back to a diverged-only
/// recovery point would delete every non-diverged ledger path.
pub(super) fn append_recovery(workspace: &Path, task_id: &str, diverged: &[PathBuf]) -> Result<()> {
    let refs = list_task_refs(workspace, task_id)?;
    if refs.is_empty() {
        return Ok(());
    }
    let chain = CheckpointChain::new(workspace.to_path_buf(), task_id.to_string());
    let tip_seq = refs.keys().copied().max().unwrap_or(BASE_SEQ);
    let tip_commit = refs.get(&tip_seq).cloned();
    // Start from the tip's full tree so every non-diverged ledger path keeps its
    // last-checkpoint blob; only the diverged paths are overlaid/removed.
    let mut entries: BTreeMap<Vec<u8>, Blob> = match &tip_commit {
        Some(commit) => chain.tree_entries(commit)?,
        None => BTreeMap::new(),
    };
    for path in diverged {
        let rel = chain.rel_bytes(path)?;
        match std::fs::read(path) {
            Ok(bytes) => {
                let blob = chain.write_blob(path, &bytes, Mode::of(path))?;
                entries.insert(rel, blob);
            }
            Err(_) => {
                // The path diverged by vanishing from disk: drop it from the
                // snapshot so a rollback to recovery reproduces the deletion.
                entries.remove(&rel);
            }
        }
    }
    let tree = chain.build_tree_raw(entries.into_iter())?;
    let commit = chain.commit_tree(&tree, tip_commit.as_deref(), "iris-checkpoint recovery")?;
    chain.update_ref(tip_seq + 1, &commit)?;
    Ok(())
}

/// Delete every ref in a task's namespace without a live chain (expiry sweep /
/// full teardown). Scoped to `<PREFIX>/<task-id>/` only, so no foreign ref is
/// ever touched.
pub(super) fn destroy_task_refs(workspace: &Path, task_id: &str) -> Result<()> {
    let refs = list_task_refs(workspace, task_id)?;
    for &seq in refs.keys() {
        let name = format!("{PREFIX}/{task_id}/{seq:010}");
        git_stdout(workspace, &["update-ref", "-d", &name])?;
    }
    Ok(())
}

/// Enumerate the task's checkpoint refs as `seq -> commit`. Scoped to the task
/// namespace with an exact prefix so no foreign ref is ever listed.
fn list_task_refs(workspace: &Path, task_id: &str) -> Result<BTreeMap<u64, String>> {
    let prefix = format!("{PREFIX}/{task_id}/");
    let out = git_stdout(
        workspace,
        &["for-each-ref", "--format=%(refname) %(objectname)", &prefix],
    )?;
    let text = String::from_utf8_lossy(&out);
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let Some((name, commit)) = line.split_once(' ') else {
            continue;
        };
        let Some(seq_str) = name.strip_prefix(&prefix) else {
            continue;
        };
        if let Ok(seq) = seq_str.parse::<u64>() {
            map.insert(seq, commit.trim().to_string());
        }
    }
    Ok(map)
}

/// Write `bytes` to `path` with `mode`, creating parent directories. The
/// restore primitive: a create/edit writes bytes; a delete is handled by the
/// caller removing the file.
fn write_file(path: &Path, bytes: &[u8], mode: Mode) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to recreate parent for {}", path.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("failed to restore {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bits = match mode {
            Mode::Exec => 0o755,
            Mode::Normal => 0o644,
        };
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits));
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
    }
    Ok(())
}

/// A JSON metadata trailer on the checkpoint commit message (ADR-0028: the
/// op-log metadata lives in the commit). Human-readable subject + a structured
/// trailer future tooling (the #264 diff) can parse.
fn commit_message(turn: u64, tool_call: Option<&str>, label: &str) -> String {
    let trailer = serde_json::json!({
        "turn": turn,
        "tool_call": tool_call,
        "label": label,
    });
    format!("iris-checkpoint: {label}\n\nIris-Checkpoint: {trailer}\n")
}

/// A throwaway index file for tree construction, removed on drop. Never the
/// user's `.git/index`: the checkpoint plumbing sets `GIT_INDEX_FILE` to this
/// path so `update-index`/`write-tree` operate on scratch state only.
struct TempIndex {
    path: PathBuf,
}

impl TempIndex {
    fn new(task_id: &str) -> Self {
        let nonce = rand::random::<u64>();
        let path = std::env::temp_dir().join(format!("iris-ckpt-index-{task_id}-{nonce}"));
        // Ensure a stale file never leaks into the fresh index.
        let _ = std::fs::remove_file(&path);
        Self { path }
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
