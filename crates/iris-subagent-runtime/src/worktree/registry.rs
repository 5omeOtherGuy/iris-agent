use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rand::random;

use crate::{RuntimeError, SCHEMA_VERSION, WorktreeId};

use super::WorktreeRecord;

#[derive(Debug, Clone)]
pub(crate) struct WorktreeRegistry {
    path: PathBuf,
    lock_path: PathBuf,
}

impl WorktreeRegistry {
    pub fn open(root: &Path) -> Result<Self, RuntimeError> {
        fs::create_dir_all(root).map_err(|source| RuntimeError::persistence(root, source))?;
        let path = root.join("registry.jsonl");
        let lock_path = root.join("registry.lock");
        if !path.exists() {
            File::create(&path).map_err(|source| RuntimeError::persistence(&path, source))?;
        }
        if !lock_path.exists() {
            File::create(&lock_path)
                .map_err(|source| RuntimeError::persistence(&lock_path, source))?;
        }
        Ok(Self { path, lock_path })
    }

    pub fn append(&self, record: &WorktreeRecord) -> Result<(), RuntimeError> {
        if record.schema_version != SCHEMA_VERSION {
            return Err(RuntimeError::InvalidRequest(format!(
                "unsupported worktree record schema {}",
                record.schema_version
            )));
        }
        let lock_file = self.lock_exclusive()?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|source| RuntimeError::persistence(&self.path, source))?;
        let mut line = serde_json::to_vec(record).map_err(|error| RuntimeError::CorruptRecord {
            path: self.path.clone(),
            message: error.to_string(),
        })?;
        line.push(b'\n');
        file.write_all(&line)
            .and_then(|()| file.sync_data())
            .map_err(|source| RuntimeError::persistence(&self.path, source))?;
        let _ = lock_file.unlock();
        Ok(())
    }

    pub fn latest(&self) -> Result<BTreeMap<WorktreeId, WorktreeRecord>, RuntimeError> {
        let lock_file = self.lock_shared()?;
        let mut file = File::open(&self.path)
            .map_err(|source| RuntimeError::persistence(&self.path, source))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| RuntimeError::persistence(&self.path, source))?;
        let _ = lock_file.unlock();
        parse_latest(&self.path, &bytes)
    }

    pub fn rebuild(&self, records: &[WorktreeRecord]) -> Result<(), RuntimeError> {
        let lock_file = self.lock_exclusive()?;
        let temp = self
            .path
            .with_extension(format!("tmp-{:032x}", random::<u128>()));
        let outcome = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|source| RuntimeError::persistence(&temp, source))?;
            for record in records {
                let mut line =
                    serde_json::to_vec(record).map_err(|error| RuntimeError::CorruptRecord {
                        path: temp.clone(),
                        message: error.to_string(),
                    })?;
                line.push(b'\n');
                file.write_all(&line)
                    .map_err(|source| RuntimeError::persistence(&temp, source))?;
            }
            file.sync_all()
                .map_err(|source| RuntimeError::persistence(&temp, source))?;
            fs::rename(&temp, &self.path)
                .map_err(|source| RuntimeError::persistence(&self.path, source))?;
            if let Some(parent) = self.path.parent() {
                File::open(parent)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|source| RuntimeError::persistence(parent, source))?;
            }
            Ok(())
        })();
        if outcome.is_err() {
            let _ = fs::remove_file(&temp);
        }
        let _ = lock_file.unlock();
        outcome
    }

    fn lock_exclusive(&self) -> Result<File, RuntimeError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .map_err(|source| RuntimeError::persistence(&self.lock_path, source))?;
        file.lock()
            .map_err(|source| RuntimeError::persistence(&self.lock_path, source))?;
        Ok(file)
    }

    fn lock_shared(&self) -> Result<File, RuntimeError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .map_err(|source| RuntimeError::persistence(&self.lock_path, source))?;
        file.lock_shared()
            .map_err(|source| RuntimeError::persistence(&self.lock_path, source))?;
        Ok(file)
    }
}

fn parse_latest(
    path: &Path,
    bytes: &[u8],
) -> Result<BTreeMap<WorktreeId, WorktreeRecord>, RuntimeError> {
    let complete_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let mut latest = BTreeMap::new();
    for (index, line) in bytes[..complete_len]
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        if line.is_empty() {
            continue;
        }
        let record: WorktreeRecord =
            serde_json::from_slice(line).map_err(|error| RuntimeError::CorruptRecord {
                path: path.to_path_buf(),
                message: format!("complete line {}: {error}", index + 1),
            })?;
        if record.schema_version != SCHEMA_VERSION {
            return Err(RuntimeError::CorruptRecord {
                path: path.to_path_buf(),
                message: format!("unsupported schema version {}", record.schema_version),
            });
        }
        latest.insert(record.id.clone(), record);
    }
    Ok(latest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::{CreationMode, WorktreeKind, WorktreeStatus};
    use crate::{HostPayload, InstanceId};

    fn record(root: &Path, id: WorktreeId, status: WorktreeStatus) -> WorktreeRecord {
        WorktreeRecord {
            schema_version: SCHEMA_VERSION,
            id: id.clone(),
            path: root.join(id.as_str()),
            source_repo: root.join("source"),
            repo_name: "source".to_string(),
            kind: WorktreeKind::Worker,
            creation_mode: CreationMode::Linked,
            git_ref: None,
            base_commit: "abc".to_string(),
            session_id: None,
            worker_id: None,
            group_id: None,
            selected: false,
            applied_to_parent: false,
            parent_worker_id: None,
            owner_pid: 1,
            owner_instance_id: InstanceId::new(),
            created_at_ms: 1,
            last_accessed_at_ms: None,
            status,
            metadata: HostPayload::default(),
        }
    }

    #[test]
    fn latest_wins_and_partial_tail_is_ignored() {
        let root = std::env::temp_dir().join(format!("iris-registry-{:032x}", random::<u128>()));
        let registry = WorktreeRegistry::open(&root).unwrap();
        let id = WorktreeId::new();
        registry
            .append(&record(&root, id.clone(), WorktreeStatus::Alive))
            .unwrap();
        registry
            .append(&record(&root, id.clone(), WorktreeStatus::Adoptable))
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(root.join("registry.jsonl"))
            .unwrap()
            .write_all(b"{partial")
            .unwrap();
        assert_eq!(
            registry.latest().unwrap()[&id].status,
            WorktreeStatus::Adoptable
        );
        fs::remove_dir_all(root).unwrap();
    }
}
