use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    GroupId, RecoveryPolicy, RuntimeError, SCHEMA_VERSION, WorkerEvent, WorkerId, WorkerRequest,
    WorkerResult,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub(crate) enum JournalRecord {
    Accepted {
        schema_version: u32,
        worker_id: WorkerId,
        group_id: Option<GroupId>,
        request: WorkerRequest,
    },
    Event {
        schema_version: u32,
        event: WorkerEvent,
    },
    Terminal {
        schema_version: u32,
        result: WorkerResult,
    },
    Group {
        schema_version: u32,
        group_id: GroupId,
        workers: Vec<WorkerId>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct RecoveredWorker {
    pub request: WorkerRequest,
    pub group_id: Option<GroupId>,
    pub events: Vec<WorkerEvent>,
    pub result: Option<WorkerResult>,
}

#[derive(Debug, Default)]
pub(crate) struct RecoveredState {
    pub workers: BTreeMap<WorkerId, RecoveredWorker>,
    pub groups: BTreeMap<GroupId, Vec<WorkerId>>,
}

#[derive(Debug, Clone)]
pub(crate) struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn open(state_dir: &Path) -> Result<Self, RuntimeError> {
        fs::create_dir_all(state_dir)
            .map_err(|source| RuntimeError::persistence(state_dir, source))?;
        let path = state_dir.join("runtime.jsonl");
        if !path.exists() {
            File::create(&path).map_err(|source| RuntimeError::persistence(&path, source))?;
        }
        Ok(Self { path })
    }

    pub fn accept(
        &self,
        worker_id: &WorkerId,
        group_id: Option<&GroupId>,
        request: &WorkerRequest,
        event: &WorkerEvent,
    ) -> Result<(), RuntimeError> {
        let records = [
            JournalRecord::Accepted {
                schema_version: SCHEMA_VERSION,
                worker_id: worker_id.clone(),
                group_id: group_id.cloned(),
                request: request.clone(),
            },
            JournalRecord::Event {
                schema_version: SCHEMA_VERSION,
                event: event.clone(),
            },
        ];
        self.append_many(&records)
    }

    pub fn append(&self, record: &JournalRecord) -> Result<(), RuntimeError> {
        self.append_many(std::slice::from_ref(record))
    }

    fn append_many(&self, records: &[JournalRecord]) -> Result<(), RuntimeError> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.path)
            .map_err(|source| RuntimeError::persistence(&self.path, source))?;
        file.lock()
            .map_err(|source| RuntimeError::persistence(&self.path, source))?;
        let outcome = (|| {
            for record in records {
                let mut line =
                    serde_json::to_vec(record).map_err(|error| RuntimeError::CorruptRecord {
                        path: self.path.clone(),
                        message: error.to_string(),
                    })?;
                line.push(b'\n');
                file.write_all(&line)
                    .map_err(|source| RuntimeError::persistence(&self.path, source))?;
            }
            file.sync_data()
                .map_err(|source| RuntimeError::persistence(&self.path, source))
        })();
        let _ = file.unlock();
        outcome
    }

    pub fn recover(&self) -> Result<RecoveredState, RuntimeError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|source| RuntimeError::persistence(&self.path, source))?;
        file.lock_shared()
            .map_err(|source| RuntimeError::persistence(&self.path, source))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| RuntimeError::persistence(&self.path, source))?;
        let _ = file.unlock();

        let complete_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let mut state = RecoveredState::default();
        for (index, line) in bytes[..complete_len]
            .split(|byte| *byte == b'\n')
            .enumerate()
        {
            if line.is_empty() {
                continue;
            }
            let record: JournalRecord =
                serde_json::from_slice(line).map_err(|error| RuntimeError::CorruptRecord {
                    path: self.path.clone(),
                    message: format!("complete line {}: {error}", index + 1),
                })?;
            match record {
                JournalRecord::Accepted {
                    schema_version,
                    worker_id,
                    group_id,
                    request,
                } => {
                    check_schema(&self.path, schema_version)?;
                    state.workers.insert(
                        worker_id,
                        RecoveredWorker {
                            request,
                            group_id,
                            events: Vec::new(),
                            result: None,
                        },
                    );
                }
                JournalRecord::Event {
                    schema_version,
                    event,
                } => {
                    check_schema(&self.path, schema_version)?;
                    if let Some(worker) = state.workers.get_mut(&event.worker_id) {
                        worker.events.push(event);
                    }
                }
                JournalRecord::Terminal {
                    schema_version,
                    result,
                } => {
                    check_schema(&self.path, schema_version)?;
                    if let Some(worker) = state.workers.get_mut(&result.worker_id) {
                        worker.result = Some(result);
                    }
                }
                JournalRecord::Group {
                    schema_version,
                    group_id,
                    workers,
                } => {
                    check_schema(&self.path, schema_version)?;
                    state.groups.insert(group_id, workers);
                }
            }
        }
        Ok(state)
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn check_schema(path: &Path, schema: u32) -> Result<(), RuntimeError> {
    if schema == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(RuntimeError::CorruptRecord {
            path: path.to_path_buf(),
            message: format!("unsupported schema version {schema}"),
        })
    }
}

pub(crate) fn recovered_status(policy: RecoveryPolicy, has_worktree: bool) -> crate::WorkerStatus {
    match policy {
        RecoveryPolicy::Adoptable if has_worktree => crate::WorkerStatus::Adoptable,
        RecoveryPolicy::Adoptable | RecoveryPolicy::Discard => crate::WorkerStatus::Interrupted,
        RecoveryPolicy::Fail => crate::WorkerStatus::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WorkerEventKind, WorkerStatus};
    use rand::random;

    #[test]
    fn partial_last_line_is_ignored() {
        let root = std::env::temp_dir().join(format!("iris-journal-{:032x}", random::<u128>()));
        let journal = Journal::open(&root).unwrap();
        let id = WorkerId::new();
        let request = WorkerRequest::read_only("run");
        let event = WorkerEvent {
            schema_version: SCHEMA_VERSION,
            worker_id: id.clone(),
            sequence: 1,
            timestamp_ms: 1,
            kind: WorkerEventKind::Status(WorkerStatus::Queued),
        };
        journal.accept(&id, None, &request, &event).unwrap();
        OpenOptions::new()
            .append(true)
            .open(journal.path())
            .unwrap()
            .write_all(b"{partial")
            .unwrap();
        let recovered = journal.recover().unwrap();
        assert_eq!(recovered.workers.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}
