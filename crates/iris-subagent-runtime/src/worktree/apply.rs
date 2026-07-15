use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};

use rand::random;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ApplyPlanId, RuntimeError, SCHEMA_VERSION, WorktreeId};

use super::process::{WorktreeCancellation, git_spec};
use super::{MutationManifest, WorktreeService, WorktreeStatus};

fn schema_version() -> u32 {
    SCHEMA_VERSION
}

/// Filesystem object represented in an apply plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ApplyFileKind {
    /// Path does not exist.
    Missing,
    /// Regular file, including binary content.
    Regular,
    /// Symbolic link represented by its link-target bytes.
    Symlink,
    /// Gitlink/submodule entry, which apply never writes.
    Gitlink,
}

/// Reviewed file-level change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ApplyChangeKind {
    /// Path is created.
    Create,
    /// Path content, type, or executable mode changes.
    Update,
    /// Path is deleted.
    Delete,
}

/// Immutable content snapshot used for TOCTOU revalidation and rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ApplyFileState {
    /// Object kind.
    pub kind: ApplyFileKind,
    /// Git-compatible mode (`100644`, `100755`, `120000`, or `160000`).
    pub mode: u32,
    /// Complete bytes for regular files or symlink target bytes.
    #[serde(default)]
    pub content: Vec<u8>,
    /// Digest of kind, mode, and content.
    pub digest: String,
}

/// One normalized delete/create/update operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ApplyOperation {
    /// Validated workspace-relative path.
    pub path: PathBuf,
    /// Change classification.
    pub change: ApplyChangeKind,
    /// State at the recorded child base.
    pub base: ApplyFileState,
    /// Reviewed current child state.
    pub child: ApplyFileState,
    /// Reviewed current parent preimage.
    pub parent: ApplyFileState,
    /// Whether committed parent `HEAD` drifted from the recorded base at this path.
    pub base_drift: bool,
    /// Whether parent working bytes/mode differ from its current `HEAD`.
    pub dirty_parent: bool,
    /// Whether a reviewed symlink target escapes the parent tree.
    pub escaping_symlink: bool,
}

/// Persisted immutable apply plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ApplyPlan {
    /// Plan schema.
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    /// Opaque plan handle.
    pub id: ApplyPlanId,
    /// Managed child candidate.
    pub worktree_id: WorktreeId,
    /// Recorded child base.
    pub base_commit: String,
    /// Parent `HEAD` observed during planning.
    pub parent_head: String,
    /// Canonical parent root.
    pub parent_root: PathBuf,
    /// Canonical child root.
    pub child_root: PathBuf,
    /// Normalized operations in path order.
    pub operations: Vec<ApplyOperation>,
    /// SHA-256 over every preceding plan field and current child bytes.
    pub digest: String,
}

/// Operator authorization supplied at the parent-mutation boundary.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ApplyOptions {
    /// Explicit overwrite approvals for dirty or base-drifted parent paths.
    pub approved_overwrites: BTreeSet<PathBuf>,
    /// Explicit approvals for symlinks whose targets escape the parent tree.
    pub approved_escaping_symlinks: BTreeSet<PathBuf>,
    /// Explicitly skipped paths; skipped candidates remain open.
    pub skipped_paths: BTreeSet<PathBuf>,
}

impl ApplyOptions {
    /// Creates an empty authorization set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Apply conflict classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ApplyConflictKind {
    /// Parent working tree is dirty at the path.
    DirtyParent,
    /// Parent committed content drifted from the child base.
    BaseDrift,
    /// Symlink target escapes the parent tree without explicit approval.
    EscapingSymlink,
    /// Gitlink/submodule mutation is unsupported.
    Gitlink,
    /// Operator explicitly skipped the path.
    Skipped,
}

/// One path retained for later review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ApplyConflict {
    /// Relative path.
    pub path: PathBuf,
    /// Conflict reason.
    pub kind: ApplyConflictKind,
}

/// Overall apply disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ApplyDisposition {
    /// Every reviewed operation succeeded; candidate was consumed.
    Complete,
    /// Some paths were skipped/conflicted; candidate remains open.
    Partial,
    /// The unchanged completed plan had already been applied.
    AlreadyApplied,
}

/// Transactional apply result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ApplyResult {
    /// Plan handle.
    pub plan_id: ApplyPlanId,
    /// Overall disposition.
    pub disposition: ApplyDisposition,
    /// Successfully written paths.
    pub applied: Vec<PathBuf>,
    /// Paths retained with reasons.
    pub conflicts: Vec<ApplyConflict>,
}

#[derive(Debug, Clone, Serialize)]
struct PlanDigest<'a> {
    schema_version: u32,
    id: &'a ApplyPlanId,
    worktree_id: &'a WorktreeId,
    base_commit: &'a str,
    parent_head: &'a str,
    parent_root: &'a Path,
    child_root: &'a Path,
    operations: &'a [ApplyOperation],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApplyReceipt {
    schema_version: u32,
    plan_id: ApplyPlanId,
    plan_digest: String,
}

impl WorktreeService {
    /// Builds and persists an immutable, content-digested apply plan.
    pub fn plan_apply(
        &self,
        worktree_id: &WorktreeId,
        manifest: &MutationManifest,
        cancellation: &WorktreeCancellation,
    ) -> Result<ApplyPlan, RuntimeError> {
        let _settlement = self.lock_group_settlement()?;
        let record = self.show(worktree_id)?;
        if record.group_id.is_some() && !record.selected {
            return Err(RuntimeError::Conflict(
                "group candidate must be explicitly selected before apply review".to_string(),
            ));
        }
        self.validate_managed(&record, cancellation)?;
        let parent_root = record
            .source_repo
            .canonicalize()
            .map_err(|source| RuntimeError::persistence(&record.source_repo, source))?;
        let child_root = record
            .path
            .canonicalize()
            .map_err(|source| RuntimeError::persistence(&record.path, source))?;
        let parent_head = git_text(self, &parent_root, ["rev-parse", "HEAD"], cancellation)?;
        let mut operations = Vec::new();
        for path in manifest.paths() {
            validate_relative_path(&path)?;
            ensure_safe_parent_chain(&parent_root, &path)?;
            let base = git_state(self, &parent_root, &record.base_commit, &path, cancellation)?;
            let child = working_state(&child_root, &path)?;
            if child == base {
                continue;
            }
            let parent_head_state =
                git_state(self, &parent_root, &parent_head, &path, cancellation)?;
            let parent = working_state(&parent_root, &path)?;
            let change = match (base.kind, child.kind) {
                (ApplyFileKind::Missing, _) => ApplyChangeKind::Create,
                (_, ApplyFileKind::Missing) => ApplyChangeKind::Delete,
                _ => ApplyChangeKind::Update,
            };
            let escaping_symlink = child.kind == ApplyFileKind::Symlink
                && symlink_escapes(&parent_root, &path, &child.content)?;
            operations.push(ApplyOperation {
                path,
                change,
                base_drift: parent_head_state != base,
                dirty_parent: parent != parent_head_state,
                base,
                child,
                parent,
                escaping_symlink,
            });
        }
        operations.sort_by(|left, right| left.path.cmp(&right.path));
        let id = ApplyPlanId::new();
        let mut plan = ApplyPlan {
            schema_version: SCHEMA_VERSION,
            id,
            worktree_id: worktree_id.clone(),
            base_commit: record.base_commit,
            parent_head,
            parent_root,
            child_root,
            operations,
            digest: String::new(),
        };
        plan.digest = plan_digest(&plan)?;
        let path = self.plan_path(&plan.id);
        write_atomic_json(&path, &plan)?;
        Ok(plan)
    }

    /// Loads and validates a persisted apply plan handle.
    pub fn load_apply_plan(&self, id: &ApplyPlanId) -> Result<ApplyPlan, RuntimeError> {
        let path = self.plan_path(id);
        let plan: ApplyPlan = serde_json::from_slice(
            &fs::read(&path).map_err(|source| RuntimeError::persistence(&path, source))?,
        )
        .map_err(|error| RuntimeError::CorruptRecord {
            path: path.clone(),
            message: error.to_string(),
        })?;
        if plan.id != *id || plan.digest != plan_digest(&plan)? {
            return Err(RuntimeError::CorruptRecord {
                path,
                message: "apply plan ID or digest mismatch".to_string(),
            });
        }
        Ok(plan)
    }

    /// Revalidates and transactionally applies authorized file operations.
    pub fn apply(
        &self,
        plan: &ApplyPlan,
        options: &ApplyOptions,
        cancellation: &WorktreeCancellation,
    ) -> Result<ApplyResult, RuntimeError> {
        let _settlement = self.lock_group_settlement()?;
        if plan.schema_version != SCHEMA_VERSION || plan.digest != plan_digest(plan)? {
            return Err(RuntimeError::Conflict(
                "apply plan digest or schema is invalid".to_string(),
            ));
        }
        let mut record = self.show(&plan.worktree_id)?;
        if let Some(group_id) = &record.group_id {
            if !record.selected {
                return Err(RuntimeError::Conflict(
                    "apply plan candidate is no longer the selected group winner".to_string(),
                ));
            }
            let filter = super::WorktreeFilter {
                include_removed: true,
                ..super::WorktreeFilter::default()
            };
            if let Some(applied) = self.list(&filter)?.into_iter().find(|candidate| {
                candidate.id != record.id
                    && candidate.group_id.as_ref() == Some(group_id)
                    && candidate.applied_to_parent
            }) {
                return Err(RuntimeError::Conflict(format!(
                    "group {group_id} already applied candidate {}",
                    applied.id
                )));
            }
        }
        self.validate_managed(&record, cancellation)?;
        if record.path != plan.child_root
            || record.source_repo != plan.parent_root
            || record.base_commit != plan.base_commit
        {
            return Err(RuntimeError::Conflict(
                "apply plan no longer matches worktree identity".to_string(),
            ));
        }
        if let Some(receipt) = self.read_receipt(&plan.id)? {
            if receipt.plan_digest != plan.digest {
                return Err(RuntimeError::Conflict(
                    "applied plan receipt digest changed".to_string(),
                ));
            }
            if plan.operations.iter().all(|operation| {
                working_state(&plan.parent_root, &operation.path)
                    .is_ok_and(|state| state == operation.child)
            }) {
                return Ok(ApplyResult {
                    plan_id: plan.id.clone(),
                    disposition: ApplyDisposition::AlreadyApplied,
                    applied: plan
                        .operations
                        .iter()
                        .map(|operation| operation.path.clone())
                        .collect(),
                    conflicts: Vec::new(),
                });
            }
            return Err(RuntimeError::Conflict(
                "parent drifted after the recorded apply".to_string(),
            ));
        }
        let current_head = git_text(self, &plan.parent_root, ["rev-parse", "HEAD"], cancellation)?;
        if current_head != plan.parent_head {
            return Err(RuntimeError::Conflict(
                "parent HEAD changed after apply review".to_string(),
            ));
        }

        let mut selected = Vec::new();
        let mut conflicts = Vec::new();
        for operation in &plan.operations {
            validate_relative_path(&operation.path)?;
            ensure_safe_parent_chain(&plan.parent_root, &operation.path)?;
            let child = working_state(&plan.child_root, &operation.path)?;
            let parent = working_state(&plan.parent_root, &operation.path)?;
            if child != operation.child {
                return Err(RuntimeError::Conflict(format!(
                    "child bytes changed after review: {}",
                    operation.path.display()
                )));
            }
            if parent != operation.parent {
                return Err(RuntimeError::Conflict(format!(
                    "parent bytes changed after review: {}",
                    operation.path.display()
                )));
            }
            let conflict = if options.skipped_paths.contains(&operation.path) {
                Some(ApplyConflictKind::Skipped)
            } else if operation.base.kind == ApplyFileKind::Gitlink
                || operation.child.kind == ApplyFileKind::Gitlink
            {
                Some(ApplyConflictKind::Gitlink)
            } else if operation.escaping_symlink
                && !options.approved_escaping_symlinks.contains(&operation.path)
            {
                Some(ApplyConflictKind::EscapingSymlink)
            } else if operation.base_drift && !options.approved_overwrites.contains(&operation.path)
            {
                Some(ApplyConflictKind::BaseDrift)
            } else if operation.dirty_parent
                && !options.approved_overwrites.contains(&operation.path)
            {
                Some(ApplyConflictKind::DirtyParent)
            } else {
                None
            };
            if let Some(kind) = conflict {
                conflicts.push(ApplyConflict {
                    path: operation.path.clone(),
                    kind,
                });
            } else {
                selected.push(operation);
            }
        }

        // Preflight finished for every accepted operation before the first write.
        let mut applied = Vec::new();
        let mut created_dirs = Vec::new();
        for operation in selected {
            if cancellation.is_cancelled() {
                rollback(&plan.parent_root, &plan.operations, &applied, &created_dirs)?;
                return Err(RuntimeError::Conflict(
                    "apply cancelled and rolled back".to_string(),
                ));
            }
            if let Err(error) = apply_state(
                &plan.parent_root,
                &operation.path,
                &operation.child,
                &mut created_dirs,
            ) {
                rollback(&plan.parent_root, &plan.operations, &applied, &created_dirs)?;
                return Err(error);
            }
            applied.push(operation.path.clone());
        }

        let disposition = if conflicts.is_empty() {
            let receipt = ApplyReceipt {
                schema_version: SCHEMA_VERSION,
                plan_id: plan.id.clone(),
                plan_digest: plan.digest.clone(),
            };
            write_atomic_json(&self.receipt_path(&plan.id), &receipt)?;
            record.status = WorktreeStatus::Applied;
            record.applied_to_parent = true;
            self.update_record(&record)?;
            ApplyDisposition::Complete
        } else {
            ApplyDisposition::Partial
        };
        Ok(ApplyResult {
            plan_id: plan.id.clone(),
            disposition,
            applied,
            conflicts,
        })
    }

    fn plan_path(&self, id: &ApplyPlanId) -> PathBuf {
        self.root()
            .join("apply/plans")
            .join(format!("{}.json", id.as_str()))
    }

    fn receipt_path(&self, id: &ApplyPlanId) -> PathBuf {
        self.root()
            .join("apply/receipts")
            .join(format!("{}.json", id.as_str()))
    }

    fn read_receipt(&self, id: &ApplyPlanId) -> Result<Option<ApplyReceipt>, RuntimeError> {
        let path = self.receipt_path(id);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                RuntimeError::CorruptRecord {
                    path,
                    message: error.to_string(),
                }
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(RuntimeError::persistence(path, source)),
        }
    }
}

fn git_state(
    service: &WorktreeService,
    repo: &Path,
    commit: &str,
    path: &Path,
    cancellation: &WorktreeCancellation,
) -> Result<ApplyFileState, RuntimeError> {
    let output = service.runner().run(
        &git_spec(
            repo.to_path_buf(),
            service.process_timeout(),
            [
                "ls-tree".to_string(),
                "-z".to_string(),
                commit.to_string(),
                "--".to_string(),
                path.to_string_lossy().to_string(),
            ],
        ),
        cancellation,
    )?;
    if output.status != 0 {
        return Err(RuntimeError::Process {
            program: "git".to_string(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    if output.stdout.is_empty() {
        return Ok(file_state(ApplyFileKind::Missing, 0, Vec::new()));
    }
    let header_end = output
        .stdout
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| RuntimeError::CorruptRecord {
            path: repo.to_path_buf(),
            message: "git ls-tree output missing path separator".to_string(),
        })?;
    let header = String::from_utf8_lossy(&output.stdout[..header_end]);
    let mut fields = header.split_whitespace();
    let mode = u32::from_str_radix(fields.next().unwrap_or(""), 8).map_err(|error| {
        RuntimeError::CorruptRecord {
            path: repo.to_path_buf(),
            message: format!("invalid git mode: {error}"),
        }
    })?;
    if mode == 0o160000 {
        return Ok(file_state(ApplyFileKind::Gitlink, mode, Vec::new()));
    }
    let kind = if mode == 0o120000 {
        ApplyFileKind::Symlink
    } else {
        ApplyFileKind::Regular
    };
    let output = service.runner().run(
        &git_spec(
            repo.to_path_buf(),
            service.process_timeout(),
            [
                "show".to_string(),
                format!("{commit}:{}", path.to_string_lossy()),
            ],
        ),
        cancellation,
    )?;
    if output.status != 0 {
        return Err(RuntimeError::Process {
            program: "git".to_string(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(file_state(kind, mode, output.stdout))
}

fn git_text<const N: usize>(
    service: &WorktreeService,
    repo: &Path,
    args: [&str; N],
    cancellation: &WorktreeCancellation,
) -> Result<String, RuntimeError> {
    let output = service.runner().run(
        &git_spec(repo.to_path_buf(), service.process_timeout(), args),
        cancellation,
    )?;
    output.success_text("git")
}

fn working_state(root: &Path, relative: &Path) -> Result<ApplyFileState, RuntimeError> {
    validate_relative_path(relative)?;
    let path = root.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(file_state(ApplyFileKind::Missing, 0, Vec::new()));
        }
        Err(source) => return Err(RuntimeError::persistence(path, source)),
    };
    if metadata.file_type().is_symlink() {
        let target =
            fs::read_link(&path).map_err(|source| RuntimeError::persistence(&path, source))?;
        return Ok(file_state(
            ApplyFileKind::Symlink,
            0o120000,
            os_bytes(target.as_os_str()),
        ));
    }
    if !metadata.is_file() {
        return Err(RuntimeError::Conflict(format!(
            "unsupported non-file apply path: {}",
            relative.display()
        )));
    }
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    let mode = if executable { 0o100755 } else { 0o100644 };
    let content = fs::read(&path).map_err(|source| RuntimeError::persistence(&path, source))?;
    Ok(file_state(ApplyFileKind::Regular, mode, content))
}

fn file_state(kind: ApplyFileKind, mode: u32, content: Vec<u8>) -> ApplyFileState {
    let mut hasher = Sha256::new();
    hasher.update(format!("{kind:?}:{mode:o}:").as_bytes());
    hasher.update(&content);
    ApplyFileState {
        kind,
        mode,
        content,
        digest: hex(&hasher.finalize()),
    }
}

fn validate_relative_path(path: &Path) -> Result<(), RuntimeError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || path
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == ".git")
    {
        return Err(RuntimeError::UnsafePath {
            path: path.to_path_buf(),
            reason: "apply path must be a non-.git workspace-relative normal path".to_string(),
        });
    }
    Ok(())
}

fn ensure_safe_parent_chain(root: &Path, relative: &Path) -> Result<(), RuntimeError> {
    let mut current = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(RuntimeError::UnsafePath {
                        path: current,
                        reason: "parent directory is a symlink".to_string(),
                    });
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(RuntimeError::Conflict(format!(
                        "apply parent is not a directory: {}",
                        current.display()
                    )));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(source) => return Err(RuntimeError::persistence(current, source)),
            }
        }
    }
    Ok(())
}

fn symlink_escapes(root: &Path, relative: &Path, target: &[u8]) -> Result<bool, RuntimeError> {
    let target = path_from_bytes(target)?;
    if target.is_absolute() {
        return Ok(true);
    }
    let mut depth = relative
        .parent()
        .map_or(0, |parent| parent.components().count());
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                if depth == 0 {
                    return Ok(true);
                }
                depth -= 1;
            }
            Component::RootDir | Component::Prefix(_) => return Ok(true),
        }
    }
    let joined = root
        .join(relative.parent().unwrap_or_else(|| Path::new("")))
        .join(&target);
    if joined.exists() {
        return Ok(!joined
            .canonicalize()
            .map_err(|source| RuntimeError::persistence(&joined, source))?
            .starts_with(root));
    }
    Ok(false)
}

fn apply_state(
    root: &Path,
    relative: &Path,
    state: &ApplyFileState,
    created_dirs: &mut Vec<PathBuf>,
) -> Result<(), RuntimeError> {
    let path = root.join(relative);
    if state.kind == ApplyFileKind::Missing {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                return Err(RuntimeError::Conflict(format!(
                    "refusing to delete directory {}",
                    relative.display()
                )));
            }
            Ok(_) => {
                fs::remove_file(&path).map_err(|source| RuntimeError::persistence(&path, source))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(RuntimeError::persistence(&path, source)),
        }
        return Ok(());
    }
    if matches!(state.kind, ApplyFileKind::Gitlink) {
        return Err(RuntimeError::Conflict(
            "gitlink/submodule apply is unsupported".to_string(),
        ));
    }
    create_parents(root, relative, created_dirs)?;
    let parent = path.parent().expect("validated path has parent");
    let temp = parent.join(format!(".iris-apply-{:032x}", random::<u128>()));
    match state.kind {
        ApplyFileKind::Regular => {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|source| RuntimeError::persistence(&temp, source))?;
            use std::io::Write;
            file.write_all(&state.content)
                .and_then(|()| file.sync_all())
                .map_err(|source| RuntimeError::persistence(&temp, source))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if state.mode == 0o100755 { 0o755 } else { 0o644 };
                fs::set_permissions(&temp, fs::Permissions::from_mode(mode))
                    .map_err(|source| RuntimeError::persistence(&temp, source))?;
            }
        }
        ApplyFileKind::Symlink => create_symlink(&path_from_bytes(&state.content)?, &temp)?,
        ApplyFileKind::Missing | ApplyFileKind::Gitlink => unreachable!(),
    }
    if let Err(source) = fs::rename(&temp, &path) {
        let _ = fs::remove_file(&temp);
        return Err(RuntimeError::persistence(path, source));
    }
    Ok(())
}

fn create_parents(
    root: &Path,
    relative: &Path,
    created: &mut Vec<PathBuf>,
) -> Result<(), RuntimeError> {
    let mut current = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            current.push(component.as_os_str());
            if !current.exists() {
                fs::create_dir(&current)
                    .map_err(|source| RuntimeError::persistence(&current, source))?;
                created.push(current.clone());
            }
        }
    }
    Ok(())
}

fn rollback(
    root: &Path,
    operations: &[ApplyOperation],
    applied: &[PathBuf],
    created_dirs: &[PathBuf],
) -> Result<(), RuntimeError> {
    for path in applied.iter().rev() {
        let operation = operations
            .iter()
            .find(|operation| &operation.path == path)
            .expect("applied operation exists");
        apply_state(root, path, &operation.parent, &mut Vec::new())?;
    }
    for directory in created_dirs.iter().rev() {
        let _ = fs::remove_dir(directory);
    }
    Ok(())
}

fn plan_digest(plan: &ApplyPlan) -> Result<String, RuntimeError> {
    let bytes = serde_json::to_vec(&PlanDigest {
        schema_version: plan.schema_version,
        id: &plan.id,
        worktree_id: &plan.worktree_id,
        base_commit: &plan.base_commit,
        parent_head: &plan.parent_head,
        parent_root: &plan.parent_root,
        child_root: &plan.child_root,
        operations: &plan.operations,
    })
    .map_err(|error| RuntimeError::InvalidRequest(error.to_string()))?;
    Ok(hex(&Sha256::digest(bytes)))
}

fn write_atomic_json(path: &Path, value: &impl Serialize) -> Result<(), RuntimeError> {
    let parent = path.parent().expect("apply path has parent");
    fs::create_dir_all(parent).map_err(|source| RuntimeError::persistence(parent, source))?;
    let temp = parent.join(format!(".tmp-{:032x}", random::<u128>()));
    let bytes = serde_json::to_vec(value)
        .map_err(|error| RuntimeError::InvalidRequest(error.to_string()))?;
    fs::write(&temp, bytes).map_err(|source| RuntimeError::persistence(&temp, source))?;
    File::open(&temp)
        .and_then(|file| file.sync_all())
        .map_err(|source| RuntimeError::persistence(&temp, source))?;
    fs::rename(&temp, path).map_err(|source| RuntimeError::persistence(path, source))
}

#[cfg(unix)]
fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn path_from_bytes(value: &[u8]) -> Result<PathBuf, RuntimeError> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(value)))
}

#[cfg(not(unix))]
fn path_from_bytes(value: &[u8]) -> Result<PathBuf, RuntimeError> {
    String::from_utf8(value.to_vec())
        .map(PathBuf::from)
        .map_err(|_| {
            RuntimeError::InvalidRequest("non-UTF-8 symlink target is unsupported".to_string())
        })
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> Result<(), RuntimeError> {
    std::os::unix::fs::symlink(target, link)
        .map_err(|source| RuntimeError::persistence(link, source))
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, link: &Path) -> Result<(), RuntimeError> {
    Err(RuntimeError::UnsupportedWorkspace(format!(
        "symlink apply is unsupported on this platform: {}",
        link.display()
    )))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}
