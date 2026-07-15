use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::random;
use serde::{Deserialize, Serialize};

use crate::{HostPayload, InstanceId, RuntimeError, SCHEMA_VERSION, WorktreeId};

use super::process::{
    ProcessRunner, ProcessSpec, SystemProcessRunner, WorktreeCancellation, git_spec,
};
use super::registry::WorktreeRegistry;
use super::{
    CreationMode, GcReport, RemoveOptions, RemoveOutcome, StrategyPreference, WorktreeConfig,
    WorktreeCreateRequest, WorktreeFilter, WorktreeKind, WorktreeRecord, WorktreeStatus,
};

const ADMIN_MARKER: &str = "iris-subagent-runtime.json";

/// Injectable process-liveness check. Owner lease state remains authoritative.
pub trait ProcessLiveness: Send + Sync + 'static {
    /// Returns whether the PID currently identifies a live process.
    fn is_alive(&self, pid: u32) -> bool;
}

/// Conservative system process-liveness check.
#[derive(Debug, Default)]
pub struct SystemProcessLiveness;

impl ProcessLiveness for SystemProcessLiveness {
    fn is_alive(&self, pid: u32) -> bool {
        if pid == std::process::id() {
            return true;
        }
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// Reusable managed-worktree lifecycle service.
pub struct WorktreeService {
    config: WorktreeConfig,
    root: PathBuf,
    registry: WorktreeRegistry,
    runner: Arc<dyn ProcessRunner>,
    liveness: Arc<dyn ProcessLiveness>,
    instance_id: InstanceId,
    group_settlement: Mutex<()>,
    _lease: File,
}

pub(crate) struct GroupSettlementGuard<'a> {
    _local: MutexGuard<'a, ()>,
    file: File,
}

impl Drop for GroupSettlementGuard<'_> {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl WorktreeService {
    /// Opens a service with system subprocess and liveness adapters.
    pub fn open(config: WorktreeConfig) -> Result<Self, RuntimeError> {
        Self::with_ports(
            config,
            Arc::new(SystemProcessRunner),
            Arc::new(SystemProcessLiveness),
        )
    }

    /// Opens a service with injectable process ports.
    pub fn with_ports(
        config: WorktreeConfig,
        runner: Arc<dyn ProcessRunner>,
        liveness: Arc<dyn ProcessLiveness>,
    ) -> Result<Self, RuntimeError> {
        if config.max_worktrees == 0 {
            return Err(RuntimeError::InvalidRequest(
                "max_worktrees must be non-zero".to_string(),
            ));
        }
        fs::create_dir_all(&config.root)
            .map_err(|source| RuntimeError::persistence(&config.root, source))?;
        let root = config
            .root
            .canonicalize()
            .map_err(|source| RuntimeError::persistence(&config.root, source))?;
        if root == Path::new("/") {
            return Err(RuntimeError::UnsafePath {
                path: root,
                reason: "managed root cannot be the filesystem root".to_string(),
            });
        }
        for protected in &config.protected_roots {
            let protected = protected
                .canonicalize()
                .map_err(|source| RuntimeError::persistence(protected, source))?;
            if root == protected || protected.starts_with(&root) {
                return Err(RuntimeError::UnsafePath {
                    path: root,
                    reason: format!(
                        "managed root cannot equal or contain protected root {}",
                        protected.display()
                    ),
                });
            }
        }
        fs::create_dir_all(root.join("worktrees"))
            .and_then(|()| fs::create_dir_all(root.join("control")))
            .and_then(|()| fs::create_dir_all(root.join("leases")))
            .map_err(|source| RuntimeError::persistence(&root, source))?;
        let registry = WorktreeRegistry::open(&root)?;
        let instance_id = InstanceId::new();
        let lease_path = root
            .join("leases")
            .join(format!("{}.lock", instance_id.as_str()));
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lease_path)
            .map_err(|source| RuntimeError::persistence(&lease_path, source))?;
        lease
            .lock()
            .map_err(|source| RuntimeError::persistence(&lease_path, source))?;
        Ok(Self {
            config,
            root,
            registry,
            runner,
            liveness,
            instance_id,
            group_settlement: Mutex::new(()),
            _lease: lease,
        })
    }

    /// Returns this service owner's collision-safe lease ID.
    #[must_use]
    pub fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    pub(crate) fn lock_group_settlement(&self) -> Result<GroupSettlementGuard<'_>, RuntimeError> {
        let local = self.group_settlement.lock().map_err(|_| {
            RuntimeError::Conflict("group settlement state is poisoned".to_string())
        })?;
        let path = self.root.join("group-settlement.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| RuntimeError::persistence(&path, source))?;
        file.lock()
            .map_err(|source| RuntimeError::persistence(&path, source))?;
        Ok(GroupSettlementGuard {
            _local: local,
            file,
        })
    }

    /// Creates a detached managed worktree at an exact base commit.
    pub fn create(
        &self,
        request: WorktreeCreateRequest,
        cancellation: &WorktreeCancellation,
    ) -> Result<WorktreeRecord, RuntimeError> {
        self.ensure_capacity()?;
        let source_input = request
            .source
            .canonicalize()
            .map_err(|source| RuntimeError::persistence(&request.source, source))?;
        if detect_jj(&source_input) {
            return Err(RuntimeError::UnsupportedWorkspace(
                "jj workspace creation is not supported; refusing git fallback".to_string(),
            ));
        }
        let source_repo = self.git_text(
            &source_input,
            ["rev-parse", "--show-toplevel"],
            cancellation,
        )?;
        let source_repo = PathBuf::from(source_repo)
            .canonicalize()
            .map_err(|source| RuntimeError::persistence(&source_input, source))?;
        let bare = self.git_text(
            &source_input,
            ["rev-parse", "--is-bare-repository"],
            cancellation,
        )?;
        if bare == "true" {
            return Err(RuntimeError::UnsupportedWorkspace(
                "bare repository has no working tree".to_string(),
            ));
        }
        if source_repo == self.root
            || source_repo.starts_with(&self.root)
            || self.root.starts_with(&source_repo)
        {
            return Err(RuntimeError::UnsafePath {
                path: self.root.clone(),
                reason: "managed root and source repository must not contain each other"
                    .to_string(),
            });
        }
        let requested_ref = request.base.clone().unwrap_or_else(|| "HEAD".to_string());
        let base_commit = self.git_text(
            &source_input,
            ["rev-parse", &format!("{requested_ref}^{{commit}}")],
            cancellation,
        )?;
        if base_commit.len() != 40 && base_commit.len() != 64 {
            return Err(RuntimeError::Process {
                program: "git".to_string(),
                message: "resolved base is not a full object ID".to_string(),
            });
        }

        let id = WorktreeId::new();
        let path = self.root.join("worktrees").join(id.as_str());
        if path.exists() {
            return Err(RuntimeError::Conflict(format!(
                "managed worktree path already exists: {}",
                path.display()
            )));
        }
        let mode = if matches!(
            request.strategy,
            StrategyPreference::Auto | StrategyPreference::BtrfsPreferred
        ) && self.btrfs_eligible(&source_input, &source_repo, cancellation)
        {
            match self.create_btrfs(&source_repo, &path, cancellation) {
                Ok(()) => CreationMode::BtrfsSnapshot,
                Err(_) => {
                    self.cleanup_partial_btrfs(&path, cancellation);
                    self.create_linked(&source_repo, &path, &base_commit, cancellation)?;
                    CreationMode::Linked
                }
            }
        } else {
            self.create_linked(&source_repo, &path, &base_commit, cancellation)?;
            CreationMode::Linked
        };

        let canonical_path = path.canonicalize().map_err(|source| {
            self.cleanup_created(mode, &source_repo, &path, cancellation);
            RuntimeError::persistence(&path, source)
        })?;
        let record = WorktreeRecord {
            schema_version: SCHEMA_VERSION,
            id,
            path: canonical_path,
            source_repo: source_repo.clone(),
            repo_name: source_repo
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("repository")
                .to_string(),
            kind: request.kind,
            creation_mode: mode,
            git_ref: Some(requested_ref),
            base_commit,
            session_id: request.session_id,
            worker_id: request.worker_id,
            group_id: request.group_id,
            selected: false,
            applied_to_parent: false,
            parent_worker_id: request.parent_worker_id,
            owner_pid: std::process::id(),
            owner_instance_id: self.instance_id.clone(),
            created_at_ms: now_ms(),
            last_accessed_at_ms: None,
            status: WorktreeStatus::Alive,
            metadata: request.metadata,
        };
        if let Err(error) = self.write_markers(&record) {
            self.cleanup_created(mode, &source_repo, &record.path, cancellation);
            return Err(error);
        }
        if let Err(error) = self.registry.append(&record) {
            self.cleanup_created(mode, &source_repo, &record.path, cancellation);
            let _ = fs::remove_file(self.control_path(&record.id));
            return Err(error);
        }
        Ok(record)
    }

    /// Lists latest records matching a filter.
    pub fn list(&self, filter: &WorktreeFilter) -> Result<Vec<WorktreeRecord>, RuntimeError> {
        let mut records = self
            .registry
            .latest()?
            .into_values()
            .filter(|record| {
                (filter.include_removed || record.status != WorktreeStatus::Removed)
                    && filter.kind.is_none_or(|kind| record.kind == kind)
                    && filter.status.is_none_or(|status| record.status == status)
                    && filter.source_repo.as_ref().is_none_or(|source| {
                        source
                            .canonicalize()
                            .is_ok_and(|source| source == record.source_repo)
                    })
            })
            .collect::<Vec<_>>();
        records.sort_by_key(|record| std::cmp::Reverse(record.created_at_ms));
        Ok(records)
    }

    /// Shows one latest record.
    pub fn show(&self, id: &WorktreeId) -> Result<WorktreeRecord, RuntimeError> {
        self.registry
            .latest()?
            .remove(id)
            .ok_or_else(|| RuntimeError::NotFound {
                kind: "worktree",
                id: id.to_string(),
            })
    }

    /// Removes a structurally verified managed worktree.
    pub fn remove(
        &self,
        id: &WorktreeId,
        options: RemoveOptions,
        cancellation: &WorktreeCancellation,
    ) -> Result<RemoveOutcome, RuntimeError> {
        let mut record = self.show(id)?;
        if record.status == WorktreeStatus::Removed {
            return Ok(RemoveOutcome::AlreadyRemoved);
        }
        self.validate_managed(&record, cancellation)?;
        if !options.force && self.owner_is_live(&record) {
            return Err(RuntimeError::Conflict(format!(
                "worktree {} is owned by a live instance",
                record.id
            )));
        }
        if options.dry_run {
            return Ok(RemoveOutcome::WouldRemove(record.path));
        }
        match record.creation_mode {
            CreationMode::Linked => {
                let mut args = vec!["worktree".to_string(), "remove".to_string()];
                if options.force {
                    args.push("--force".to_string());
                }
                args.push(record.path.to_string_lossy().to_string());
                self.git_output(&record.source_repo, args, cancellation)?;
            }
            CreationMode::BtrfsSnapshot => {
                self.run_success(
                    ProcessSpec {
                        program: "btrfs".to_string(),
                        args: vec![
                            "subvolume".to_string(),
                            "delete".to_string(),
                            record.path.to_string_lossy().to_string(),
                        ],
                        cwd: Some(self.root.clone()),
                        timeout: self.config.process_timeout,
                        env: Vec::new(),
                    },
                    cancellation,
                )?;
            }
        }
        let removed_path = record.path.clone();
        let _ = fs::remove_file(self.control_path(&record.id));
        record.status = WorktreeStatus::Removed;
        record.last_accessed_at_ms = Some(now_ms());
        self.registry.append(&record)?;
        self.prune_if_safe(&record.source_repo, cancellation)?;
        Ok(RemoveOutcome::Removed(removed_path))
    }

    /// Marks dead-owner valid worktrees adoptable and never deletes them automatically.
    pub fn gc(
        &self,
        options: RemoveOptions,
        cancellation: &WorktreeCancellation,
    ) -> Result<GcReport, RuntimeError> {
        let mut report = GcReport::default();
        let records = self.list(&WorktreeFilter::default())?;
        for mut record in records {
            if record.status == WorktreeStatus::Ignored || record.status == WorktreeStatus::Applied
            {
                continue;
            }
            if self.owner_is_live(&record) && !options.force {
                report.skipped_live.push(record.id.clone());
                continue;
            }
            if self.validate_managed(&record, cancellation).is_err() {
                report.corrupt.push(record.id.clone());
                continue;
            }
            if record.status != WorktreeStatus::Adoptable {
                record.status = WorktreeStatus::Adoptable;
                record.last_accessed_at_ms = Some(now_ms());
                if !options.dry_run {
                    self.update_record(&record)?;
                }
            }
            report.adoptable.push(record.id);
        }
        report.prune_suppressed = !report.adoptable.is_empty();
        Ok(report)
    }

    /// Explicitly adopts a valid dead-owner worktree under this instance lease.
    pub fn adopt(
        &self,
        id: &WorktreeId,
        cancellation: &WorktreeCancellation,
    ) -> Result<WorktreeRecord, RuntimeError> {
        let mut record = self.show(id)?;
        if self.owner_is_live(&record) {
            return Err(RuntimeError::Conflict(
                "cannot adopt a live-owner worktree".to_string(),
            ));
        }
        self.validate_managed(&record, cancellation)?;
        record.owner_pid = std::process::id();
        record.owner_instance_id = self.instance_id.clone();
        record.status = WorktreeStatus::Alive;
        record.last_accessed_at_ms = Some(now_ms());
        self.update_record(&record)?;
        Ok(record)
    }

    /// Explicitly ignores an adoptable candidate without deleting it.
    pub fn ignore(&self, id: &WorktreeId) -> Result<WorktreeRecord, RuntimeError> {
        let mut record = self.show(id)?;
        if record.status != WorktreeStatus::Adoptable {
            return Err(RuntimeError::Conflict(
                "only adoptable worktrees can be ignored".to_string(),
            ));
        }
        record.status = WorktreeStatus::Ignored;
        record.last_accessed_at_ms = Some(now_ms());
        self.update_record(&record)?;
        Ok(record)
    }

    /// Durably selects one member of a worktree group for reviewed apply.
    /// Selection may change before apply; a completed group apply is final.
    pub fn select_group_candidate(&self, id: &WorktreeId) -> Result<WorktreeRecord, RuntimeError> {
        let _settlement = self.lock_group_settlement()?;
        let mut target = self.show(id)?;
        let group_id = target.group_id.clone().ok_or_else(|| {
            RuntimeError::Conflict("worktree is not a group candidate".to_string())
        })?;
        let filter = WorktreeFilter {
            include_removed: true,
            ..WorktreeFilter::default()
        };
        let mut members = self
            .list(&filter)?
            .into_iter()
            .filter(|record| record.group_id.as_ref() == Some(&group_id))
            .collect::<Vec<_>>();
        if let Some(applied) = members.iter().find(|record| record.applied_to_parent) {
            if applied.id == target.id {
                return Ok(applied.clone());
            }
            return Err(RuntimeError::Conflict(format!(
                "group {group_id} already applied candidate {}",
                applied.id
            )));
        }
        if target.status != WorktreeStatus::Alive {
            return Err(RuntimeError::Conflict(
                "only a live group candidate can be selected".to_string(),
            ));
        }
        for member in &mut members {
            if member.id != target.id && member.selected {
                member.selected = false;
                self.update_record(member)?;
            }
        }
        if !target.selected {
            target.selected = true;
            target.last_accessed_at_ms = Some(now_ms());
            self.update_record(&target)?;
        }
        Ok(target)
    }

    /// Rebuilds the JSONL registry from validated control records.
    pub fn rebuild(
        &self,
        cancellation: &WorktreeCancellation,
    ) -> Result<Vec<WorktreeRecord>, RuntimeError> {
        let mut records = Vec::new();
        for entry in fs::read_dir(self.root.join("control"))
            .map_err(|source| RuntimeError::persistence(self.root.join("control"), source))?
        {
            let entry = entry.map_err(|source| RuntimeError::persistence(&self.root, source))?;
            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let record: WorktreeRecord = serde_json::from_slice(
                &fs::read(entry.path())
                    .map_err(|source| RuntimeError::persistence(entry.path(), source))?,
            )
            .map_err(|error| RuntimeError::CorruptRecord {
                path: entry.path(),
                message: error.to_string(),
            })?;
            self.validate_managed(&record, cancellation)?;
            records.push(record);
        }
        records.sort_by_key(|record| record.created_at_ms);
        self.registry.rebuild(&records)?;
        Ok(records)
    }

    /// Creates up to the configured count of pristine pool worktrees.
    pub fn prewarm(
        &self,
        source: impl Into<PathBuf>,
        count: usize,
        cancellation: &WorktreeCancellation,
    ) -> Result<Vec<WorktreeRecord>, RuntimeError> {
        let count = count.min(self.config.max_pool_size);
        let source = source.into();
        let mut records = Vec::with_capacity(count);
        for _ in 0..count {
            let mut request = WorktreeCreateRequest::worker(source.clone());
            request.kind = WorktreeKind::Pool;
            records.push(self.create(request, cancellation)?);
        }
        Ok(records)
    }

    /// Acquires only a structurally valid, pristine prewarmed worktree.
    pub fn acquire_pooled(
        &self,
        source: &Path,
        cancellation: &WorktreeCancellation,
    ) -> Result<Option<WorktreeRecord>, RuntimeError> {
        let source = source
            .canonicalize()
            .map_err(|error| RuntimeError::persistence(source, error))?;
        for mut record in self.list(&WorktreeFilter {
            source_repo: Some(source),
            kind: Some(WorktreeKind::Pool),
            status: Some(WorktreeStatus::Alive),
            include_removed: false,
        })? {
            self.validate_managed(&record, cancellation)?;
            if self.is_pristine(&record, cancellation)? {
                record.kind = WorktreeKind::Worker;
                record.last_accessed_at_ms = Some(now_ms());
                self.update_record(&record)?;
                return Ok(Some(record));
            }
        }
        Ok(None)
    }

    /// Returns an unchanged worktree to the pool; changed candidates are retained.
    pub fn release_to_pool(
        &self,
        id: &WorktreeId,
        cancellation: &WorktreeCancellation,
    ) -> Result<WorktreeRecord, RuntimeError> {
        let mut record = self.show(id)?;
        self.validate_managed(&record, cancellation)?;
        if record.status != WorktreeStatus::Alive || record.applied_to_parent {
            return Err(RuntimeError::Conflict(
                "only an unapplied live worktree may return to the pool".to_string(),
            ));
        }
        if !self.is_pristine(&record, cancellation)? {
            return Err(RuntimeError::Conflict(
                "changed worktree retained; only pristine candidates may return to pool"
                    .to_string(),
            ));
        }
        record.kind = WorktreeKind::Pool;
        record.worker_id = None;
        record.group_id = None;
        record.selected = false;
        record.applied_to_parent = false;
        record.parent_worker_id = None;
        record.session_id = None;
        record.metadata = HostPayload::default();
        record.last_accessed_at_ms = Some(now_ms());
        self.update_record(&record)?;
        Ok(record)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn runner(&self) -> &dyn ProcessRunner {
        self.runner.as_ref()
    }

    pub(crate) fn process_timeout(&self) -> std::time::Duration {
        self.config.process_timeout
    }

    fn ensure_capacity(&self) -> Result<(), RuntimeError> {
        let count = self
            .list(&WorktreeFilter::default())?
            .into_iter()
            .filter(|record| record.status != WorktreeStatus::Removed)
            .count();
        if count >= self.config.max_worktrees {
            Err(RuntimeError::Backpressure {
                queue: "worktree",
                capacity: self.config.max_worktrees,
            })
        } else {
            Ok(())
        }
    }

    fn btrfs_eligible(
        &self,
        source: &Path,
        source_repo: &Path,
        cancellation: &WorktreeCancellation,
    ) -> bool {
        if source != source_repo || !source_repo.join(".git").is_dir() {
            return false;
        }
        let clean = self
            .git_output(source_repo, ["status", "--porcelain", "-z"], cancellation)
            .is_ok_and(|output| output.stdout.is_empty());
        if !clean {
            return false;
        }
        self.runner
            .run(
                &ProcessSpec {
                    program: "btrfs".to_string(),
                    args: vec![
                        "subvolume".to_string(),
                        "show".to_string(),
                        source_repo.to_string_lossy().to_string(),
                    ],
                    cwd: Some(source_repo.to_path_buf()),
                    timeout: self.config.process_timeout,
                    env: Vec::new(),
                },
                cancellation,
            )
            .is_ok_and(|output| output.status == 0)
    }

    fn create_btrfs(
        &self,
        source: &Path,
        path: &Path,
        cancellation: &WorktreeCancellation,
    ) -> Result<(), RuntimeError> {
        self.run_success(
            ProcessSpec {
                program: "btrfs".to_string(),
                args: vec![
                    "subvolume".to_string(),
                    "snapshot".to_string(),
                    source.to_string_lossy().to_string(),
                    path.to_string_lossy().to_string(),
                ],
                cwd: Some(self.root.clone()),
                timeout: self.config.process_timeout,
                env: Vec::new(),
            },
            cancellation,
        )?;
        Ok(())
    }

    fn create_linked(
        &self,
        source: &Path,
        path: &Path,
        base: &str,
        cancellation: &WorktreeCancellation,
    ) -> Result<(), RuntimeError> {
        self.git_output(
            source,
            [
                "worktree".to_string(),
                "add".to_string(),
                "--detach".to_string(),
                path.to_string_lossy().to_string(),
                base.to_string(),
            ],
            cancellation,
        )?;
        Ok(())
    }

    fn write_markers(&self, record: &WorktreeRecord) -> Result<(), RuntimeError> {
        write_atomic_json(&self.control_path(&record.id), record)?;
        write_atomic_json(&self.admin_marker_path(record)?, record)
    }

    pub(crate) fn update_record(&self, record: &WorktreeRecord) -> Result<(), RuntimeError> {
        self.write_markers(record)?;
        self.registry.append(record)
    }

    pub(crate) fn validate_managed(
        &self,
        record: &WorktreeRecord,
        cancellation: &WorktreeCancellation,
    ) -> Result<(), RuntimeError> {
        let expected = self.root.join("worktrees").join(record.id.as_str());
        let canonical = record
            .path
            .canonicalize()
            .map_err(|source| RuntimeError::persistence(&record.path, source))?;
        if canonical != record.path || canonical != expected || !canonical.starts_with(&self.root) {
            return Err(RuntimeError::UnsafePath {
                path: record.path.clone(),
                reason: "record path is not the canonical ID-derived managed path".to_string(),
            });
        }
        let control: WorktreeRecord = read_json(&self.control_path(&record.id))?;
        let admin: WorktreeRecord = read_json(&self.admin_marker_path(record)?)?;
        if control.id != record.id
            || admin.id != record.id
            || control.path != record.path
            || admin.path != record.path
            || control.source_repo != record.source_repo
            || admin.source_repo != record.source_repo
            || control.creation_mode != record.creation_mode
            || admin.creation_mode != record.creation_mode
        {
            return Err(RuntimeError::Conflict(
                "worktree ownership markers do not match registry record".to_string(),
            ));
        }
        let source = self.git_text(
            &record.source_repo,
            ["rev-parse", "--show-toplevel"],
            cancellation,
        )?;
        let source = PathBuf::from(source)
            .canonicalize()
            .map_err(|source| RuntimeError::persistence(&record.source_repo, source))?;
        if source != record.source_repo {
            return Err(RuntimeError::Conflict(
                "source repository identity changed".to_string(),
            ));
        }
        Ok(())
    }

    fn admin_marker_path(&self, record: &WorktreeRecord) -> Result<PathBuf, RuntimeError> {
        match record.creation_mode {
            CreationMode::BtrfsSnapshot => Ok(record.path.join(".git").join(ADMIN_MARKER)),
            CreationMode::Linked => {
                let dotgit = fs::read_to_string(record.path.join(".git")).map_err(|source| {
                    RuntimeError::persistence(record.path.join(".git"), source)
                })?;
                let gitdir = dotgit.trim().strip_prefix("gitdir: ").ok_or_else(|| {
                    RuntimeError::CorruptRecord {
                        path: record.path.join(".git"),
                        message: "linked worktree gitdir marker is malformed".to_string(),
                    }
                })?;
                let gitdir = PathBuf::from(gitdir)
                    .canonicalize()
                    .map_err(|source| RuntimeError::persistence(gitdir, source))?;
                Ok(gitdir.join(ADMIN_MARKER))
            }
        }
    }

    fn control_path(&self, id: &WorktreeId) -> PathBuf {
        self.root
            .join("control")
            .join(format!("{}.json", id.as_str()))
    }

    fn owner_is_live(&self, record: &WorktreeRecord) -> bool {
        let lease_path = self
            .root
            .join("leases")
            .join(format!("{}.lock", record.owner_instance_id.as_str()));
        let lease_locked = OpenOptions::new()
            .read(true)
            .write(true)
            .open(lease_path)
            .is_ok_and(|file| match file.try_lock() {
                Ok(()) => {
                    let _ = file.unlock();
                    false
                }
                Err(_) => true,
            });
        lease_locked && self.liveness.is_alive(record.owner_pid)
    }

    fn is_pristine(
        &self,
        record: &WorktreeRecord,
        cancellation: &WorktreeCancellation,
    ) -> Result<bool, RuntimeError> {
        let status = self.git_output(
            &record.path,
            ["status", "--porcelain", "--ignored", "-z"],
            cancellation,
        )?;
        Ok(status.stdout.is_empty())
    }

    fn prune_if_safe(
        &self,
        source: &Path,
        cancellation: &WorktreeCancellation,
    ) -> Result<(), RuntimeError> {
        let has_adoptable = self
            .registry
            .latest()?
            .values()
            .any(|record| record.status == WorktreeStatus::Adoptable);
        if !has_adoptable {
            self.git_output(source, ["worktree", "prune"], cancellation)?;
        }
        Ok(())
    }

    fn cleanup_created(
        &self,
        mode: CreationMode,
        source: &Path,
        path: &Path,
        cancellation: &WorktreeCancellation,
    ) {
        if !path.starts_with(self.root.join("worktrees")) {
            return;
        }
        match mode {
            CreationMode::Linked => {
                let _ = self.git_output(
                    source,
                    [
                        "worktree".to_string(),
                        "remove".to_string(),
                        "--force".to_string(),
                        path.to_string_lossy().to_string(),
                    ],
                    cancellation,
                );
            }
            CreationMode::BtrfsSnapshot => self.cleanup_partial_btrfs(path, cancellation),
        }
    }

    fn cleanup_partial_btrfs(&self, path: &Path, cancellation: &WorktreeCancellation) {
        if path.starts_with(self.root.join("worktrees")) && path.exists() {
            let _ = self.runner.run(
                &ProcessSpec {
                    program: "btrfs".to_string(),
                    args: vec![
                        "subvolume".to_string(),
                        "delete".to_string(),
                        path.to_string_lossy().to_string(),
                    ],
                    cwd: Some(self.root.clone()),
                    timeout: self.config.process_timeout,
                    env: Vec::new(),
                },
                cancellation,
            );
        }
    }

    fn git_text<const N: usize>(
        &self,
        cwd: &Path,
        args: [&str; N],
        cancellation: &WorktreeCancellation,
    ) -> Result<String, RuntimeError> {
        self.git_output(cwd, args, cancellation)?
            .success_text("git")
    }

    fn git_output(
        &self,
        cwd: &Path,
        args: impl IntoIterator<Item = impl Into<String>>,
        cancellation: &WorktreeCancellation,
    ) -> Result<super::ProcessOutput, RuntimeError> {
        let output = self.runner.run(
            &git_spec(cwd.to_path_buf(), self.config.process_timeout, args),
            cancellation,
        )?;
        if output.status != 0 {
            return Err(RuntimeError::Process {
                program: "git".to_string(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(output)
    }

    fn run_success(
        &self,
        spec: ProcessSpec,
        cancellation: &WorktreeCancellation,
    ) -> Result<super::ProcessOutput, RuntimeError> {
        let output = self.runner.run(&spec, cancellation)?;
        if output.status != 0 {
            return Err(RuntimeError::Process {
                program: spec.program,
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(output)
    }
}

fn detect_jj(path: &Path) -> bool {
    path.ancestors()
        .any(|ancestor| ancestor.join(".jj").exists())
}

fn write_atomic_json(path: &Path, value: &impl Serialize) -> Result<(), RuntimeError> {
    let parent = path.parent().ok_or_else(|| RuntimeError::UnsafePath {
        path: path.to_path_buf(),
        reason: "marker has no parent".to_string(),
    })?;
    fs::create_dir_all(parent).map_err(|source| RuntimeError::persistence(parent, source))?;
    let temp = parent.join(format!(".marker-{:032x}", random::<u128>()));
    let bytes = serde_json::to_vec(value).map_err(|error| RuntimeError::CorruptRecord {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    fs::write(&temp, bytes).map_err(|source| RuntimeError::persistence(&temp, source))?;
    File::open(&temp)
        .and_then(|file| file.sync_all())
        .map_err(|source| RuntimeError::persistence(&temp, source))?;
    fs::rename(&temp, path).map_err(|source| RuntimeError::persistence(path, source))?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, RuntimeError> {
    serde_json::from_slice(
        &fs::read(path).map_err(|source| RuntimeError::persistence(path, source))?,
    )
    .map_err(|error| RuntimeError::CorruptRecord {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
