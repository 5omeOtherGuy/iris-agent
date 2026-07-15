use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{HostPayload, RuntimeError};

use super::{
    RemoveOptions, StrategyPreference, WorktreeCancellation, WorktreeCreateRequest, WorktreeKind,
    WorktreeRecord, WorktreeService,
};

/// Trust/auth decision made by the host before remote restore access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RestoreTrust {
    /// Host authenticated the source.
    pub authenticated: bool,
    /// Host explicitly trusts materialization into this repository.
    pub trusted: bool,
}

impl RestoreTrust {
    /// Constructs an explicitly authenticated and trusted decision.
    #[must_use]
    pub const fn trusted() -> Self {
        Self {
            authenticated: true,
            trusted: true,
        }
    }
}

/// Restore archive entry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RestoreEntryKind {
    /// Regular file bytes.
    File,
    /// Symbolic link target bytes.
    Symlink,
}

/// Validated provider-neutral snapshot entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RestoreEntry {
    /// Workspace-relative destination.
    pub path: PathBuf,
    /// Entry type.
    pub kind: RestoreEntryKind,
    /// File bytes or symlink-target bytes.
    pub content: Vec<u8>,
    /// Unix permission bits for regular files.
    #[serde(default)]
    pub mode: u32,
}

impl RestoreEntry {
    /// Constructs a regular-file entry.
    #[must_use]
    pub fn file(path: impl Into<PathBuf>, content: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            kind: RestoreEntryKind::File,
            content: content.into(),
            mode: 0o644,
        }
    }

    /// Constructs a symbolic-link entry.
    #[must_use]
    pub fn symlink(path: impl Into<PathBuf>, target: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            kind: RestoreEntryKind::Symlink,
            content: target.into(),
            mode: 0,
        }
    }
}

/// Data returned by a local or remote restore source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RestoreBundle {
    /// Source-selected repository identity echoed for validation.
    pub repository_identity: String,
    /// Source-selected exact commit echoed for validation.
    pub base_commit: String,
    /// Restored host session context.
    #[serde(default)]
    pub session_context: HostPayload,
    /// Optional code snapshot. `None` requests honest clean-checkout fallback.
    #[serde(default)]
    pub snapshot: Option<Vec<RestoreEntry>>,
}

impl RestoreBundle {
    /// Constructs a context-only bundle that falls back to a clean checkout.
    #[must_use]
    pub fn context_only(
        repository_identity: impl Into<String>,
        base_commit: impl Into<String>,
        session_context: HostPayload,
    ) -> Self {
        Self {
            repository_identity: repository_identity.into(),
            base_commit: base_commit.into(),
            session_context,
            snapshot: None,
        }
    }
}

/// Input supplied to a provider-neutral restore source.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RestoreRequest {
    /// Session being restored.
    pub session_id: String,
    /// Local source repository used for isolated checkout/fallback.
    pub source_repo: PathBuf,
    /// Host-stable repository identity expected from the source.
    pub repository_identity: String,
    /// Exact recorded base commit.
    pub base_commit: String,
    /// Host trust/auth decision.
    pub trust: RestoreTrust,
    /// Maximum archive entries.
    pub max_files: usize,
    /// Maximum aggregate entry bytes.
    pub max_bytes: usize,
    /// Creation strategy for the fresh worktree.
    pub strategy: StrategyPreference,
}

impl RestoreRequest {
    /// Creates a bounded trusted local restore request.
    #[must_use]
    pub fn trusted_local(
        session_id: impl Into<String>,
        source_repo: impl Into<PathBuf>,
        repository_identity: impl Into<String>,
        base_commit: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            source_repo: source_repo.into(),
            repository_identity: repository_identity.into(),
            base_commit: base_commit.into(),
            trust: RestoreTrust::trusted(),
            max_files: 100_000,
            max_bytes: 512 * 1024 * 1024,
            strategy: StrategyPreference::Auto,
        }
    }
}

/// Host extension port for local or remote session/code restore.
pub trait RestoreSource: Send + Sync + 'static {
    /// Fetches context and optional snapshot after the host made a trust/auth decision.
    fn fetch(&self, request: &RestoreRequest) -> Result<RestoreBundle, RuntimeError>;
}

/// Restore outcome in a fresh isolated worktree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RestoreResult {
    /// Managed restore worktree.
    pub worktree: WorktreeRecord,
    /// Restored host context.
    pub session_context: HostPayload,
    /// Whether optional snapshot bytes were materialized.
    pub snapshot_restored: bool,
    /// Honest fallback reason when clean checkout was used.
    #[serde(default)]
    pub fallback_reason: Option<String>,
}

impl WorktreeService {
    /// Restores session context and optional code into a new isolated worktree.
    pub fn restore(
        &self,
        request: &RestoreRequest,
        source: &dyn RestoreSource,
        cancellation: &WorktreeCancellation,
    ) -> Result<RestoreResult, RuntimeError> {
        if !request.trust.authenticated || !request.trust.trusted {
            return Err(RuntimeError::InvalidRequest(
                "restore source must be authenticated and explicitly trusted".to_string(),
            ));
        }
        if request.session_id.trim().is_empty()
            || request.repository_identity.trim().is_empty()
            || request.base_commit.trim().is_empty()
            || request.max_files == 0
        {
            return Err(RuntimeError::InvalidRequest(
                "restore request is missing identity or has invalid bounds".to_string(),
            ));
        }
        let bundle = source.fetch(request)?;
        if bundle.repository_identity != request.repository_identity {
            return Err(RuntimeError::Conflict(
                "restore repository identity mismatch".to_string(),
            ));
        }
        let source_repo = request
            .source_repo
            .canonicalize()
            .map_err(|source| RuntimeError::persistence(&request.source_repo, source))?;
        let commit = restore_git_text(self, &source_repo, &request.base_commit, cancellation)?;
        if commit != request.base_commit || bundle.base_commit != commit {
            return Err(RuntimeError::Conflict(
                "restore commit identity mismatch".to_string(),
            ));
        }
        if let Some(entries) = &bundle.snapshot {
            validate_entries(entries, request.max_files, request.max_bytes)?;
        }

        let mut create = WorktreeCreateRequest::worker(&source_repo);
        create.base = Some(commit);
        create.strategy = request.strategy;
        create.kind = WorktreeKind::Restore;
        create.session_id = Some(request.session_id.clone());
        let record = self.create(create, cancellation)?;
        let restored = if let Some(entries) = &bundle.snapshot {
            if let Err(error) = materialize_entries(&record.path, entries) {
                let _ = self.remove(&record.id, RemoveOptions::force(), cancellation);
                return Err(error);
            }
            true
        } else {
            false
        };
        Ok(RestoreResult {
            worktree: record,
            session_context: bundle.session_context,
            snapshot_restored: restored,
            fallback_reason: (!restored).then(|| {
                "code snapshot unavailable; clean checkout at recorded commit".to_string()
            }),
        })
    }
}

fn validate_entries(
    entries: &[RestoreEntry],
    max_files: usize,
    max_bytes: usize,
) -> Result<(), RuntimeError> {
    if entries.len() > max_files {
        return Err(RuntimeError::InvalidRequest(
            "restore snapshot exceeds file-count limit".to_string(),
        ));
    }
    let mut total = 0usize;
    let mut paths = BTreeSet::new();
    for entry in entries {
        validate_restore_path(&entry.path)?;
        if !paths.insert(entry.path.clone()) {
            return Err(RuntimeError::InvalidRequest(format!(
                "duplicate restore path: {}",
                entry.path.display()
            )));
        }
        total = total.checked_add(entry.content.len()).ok_or_else(|| {
            RuntimeError::InvalidRequest("restore byte count overflow".to_string())
        })?;
        if total > max_bytes {
            return Err(RuntimeError::InvalidRequest(
                "restore snapshot exceeds byte limit".to_string(),
            ));
        }
        if entry.kind == RestoreEntryKind::Symlink
            && symlink_target_escapes(&entry.path, &entry.content)?
        {
            return Err(RuntimeError::UnsafePath {
                path: entry.path.clone(),
                reason: "restore symlink target escapes worktree".to_string(),
            });
        }
    }
    Ok(())
}

fn validate_restore_path(path: &Path) -> Result<(), RuntimeError> {
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
            reason: "restore entry must be a non-.git relative normal path".to_string(),
        });
    }
    Ok(())
}

fn symlink_target_escapes(path: &Path, target: &[u8]) -> Result<bool, RuntimeError> {
    let target = bytes_path(target)?;
    if target.is_absolute() {
        return Ok(true);
    }
    let mut depth = path
        .parent()
        .map_or(0, |parent| parent.components().count());
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return Ok(true);
                }
                depth -= 1;
            }
            Component::RootDir | Component::Prefix(_) => return Ok(true),
        }
    }
    Ok(false)
}

fn materialize_entries(root: &Path, entries: &[RestoreEntry]) -> Result<(), RuntimeError> {
    for entry in entries {
        let path = root.join(&entry.path);
        let parent = path.parent().expect("validated restore path has parent");
        ensure_no_symlink_parents(root, &entry.path)?;
        fs::create_dir_all(parent).map_err(|source| RuntimeError::persistence(parent, source))?;
        match entry.kind {
            RestoreEntryKind::File => {
                let mut options = OpenOptions::new();
                options.write(true).create(true).truncate(true);
                let mut file = options
                    .open(&path)
                    .map_err(|source| RuntimeError::persistence(&path, source))?;
                use std::io::Write;
                file.write_all(&entry.content)
                    .and_then(|()| file.sync_all())
                    .map_err(|source| RuntimeError::persistence(&path, source))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = if entry.mode & 0o111 != 0 {
                        0o755
                    } else {
                        0o644
                    };
                    fs::set_permissions(&path, fs::Permissions::from_mode(mode))
                        .map_err(|source| RuntimeError::persistence(&path, source))?;
                }
            }
            RestoreEntryKind::Symlink => {
                match fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.is_dir() => {
                        return Err(RuntimeError::Conflict(format!(
                            "restore path is a directory: {}",
                            entry.path.display()
                        )));
                    }
                    Ok(_) => fs::remove_file(&path)
                        .map_err(|source| RuntimeError::persistence(&path, source))?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => return Err(RuntimeError::persistence(&path, source)),
                }
                create_restore_symlink(&bytes_path(&entry.content)?, &path)?;
            }
        }
    }
    Ok(())
}

fn ensure_no_symlink_parents(root: &Path, relative: &Path) -> Result<(), RuntimeError> {
    let mut current = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(RuntimeError::UnsafePath {
                        path: current,
                        reason: "restore parent is a symlink".to_string(),
                    });
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(source) => return Err(RuntimeError::persistence(current, source)),
            }
        }
    }
    Ok(())
}

fn restore_git_text(
    service: &WorktreeService,
    repo: &Path,
    commit: &str,
    cancellation: &WorktreeCancellation,
) -> Result<String, RuntimeError> {
    service
        .runner()
        .run(
            &super::process::git_spec(
                repo.to_path_buf(),
                service.process_timeout(),
                ["rev-parse", &format!("{commit}^{{commit}}")],
            ),
            cancellation,
        )?
        .success_text("git")
}

#[cfg(unix)]
fn bytes_path(bytes: &[u8]) -> Result<PathBuf, RuntimeError> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn bytes_path(bytes: &[u8]) -> Result<PathBuf, RuntimeError> {
    String::from_utf8(bytes.to_vec())
        .map(PathBuf::from)
        .map_err(|_| RuntimeError::InvalidRequest("non-UTF-8 restore path unsupported".to_string()))
}

#[cfg(unix)]
fn create_restore_symlink(target: &Path, link: &Path) -> Result<(), RuntimeError> {
    std::os::unix::fs::symlink(target, link)
        .map_err(|source| RuntimeError::persistence(link, source))
}

#[cfg(not(unix))]
fn create_restore_symlink(_target: &Path, link: &Path) -> Result<(), RuntimeError> {
    Err(RuntimeError::UnsupportedWorkspace(format!(
        "restore symlink unsupported on this platform: {}",
        link.display()
    )))
}
