use crate::{GroupResult, RuntimeError, WorkerId, WorkerResult, WorkerStatus};

/// Host/operator/evaluator port for explicit best-of-N winner selection.
pub trait CandidateSelector: Send + Sync + 'static {
    /// Selects one member ID after inspecting all candidate results.
    fn select(&self, candidates: &GroupResult) -> Result<WorkerId, RuntimeError>;
}

/// Validates an explicit selection without applying an implicit winner heuristic.
pub fn select_candidate(
    candidates: &GroupResult,
    selector: &dyn CandidateSelector,
) -> Result<WorkerResult, RuntimeError> {
    let selected = selector.select(candidates)?;
    let result = candidates
        .results
        .iter()
        .find(|result| result.worker_id == selected)
        .cloned()
        .ok_or_else(|| {
            RuntimeError::InvalidRequest(format!(
                "selected worker {selected} is not a member of group {}",
                candidates.group_id
            ))
        })?;
    if result.status != WorkerStatus::Completed {
        return Err(RuntimeError::Conflict(format!(
            "selected worker {selected} did not complete successfully"
        )));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GroupId, HostPayload, SCHEMA_VERSION, Usage, WorkerResult};

    struct Pick(WorkerId);

    impl CandidateSelector for Pick {
        fn select(&self, _candidates: &GroupResult) -> Result<WorkerId, RuntimeError> {
            Ok(self.0.clone())
        }
    }

    fn result(id: WorkerId, status: WorkerStatus) -> WorkerResult {
        WorkerResult {
            schema_version: SCHEMA_VERSION,
            worker_id: id,
            status,
            summary: String::new(),
            inline_output: None,
            artifacts: Vec::new(),
            usage: Usage::default(),
            changed_paths: Vec::new(),
            worktree: None,
            apply_plan_id: None,
            host: HostPayload::default(),
            message: None,
        }
    }

    #[test]
    fn selection_is_explicit_and_must_choose_successful_member() {
        let ok = WorkerId::new();
        let failed = WorkerId::new();
        let group = GroupResult {
            group_id: GroupId::new(),
            results: vec![
                result(ok.clone(), WorkerStatus::Completed),
                result(failed.clone(), WorkerStatus::Failed),
            ],
        };
        assert_eq!(
            select_candidate(&group, &Pick(ok.clone()))
                .unwrap()
                .worker_id,
            ok
        );
        assert!(select_candidate(&group, &Pick(failed)).is_err());
        assert!(select_candidate(&group, &Pick(WorkerId::new())).is_err());
    }
}
