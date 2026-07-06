//! Bounded parallel run engine. A fixed pool of OS threads pulls cells from a
//! shared index and executes them through `iris_agent::harness::run_cell`; a
//! single writer thread (the caller's loop) owns the JSONL log so concurrent
//! cells never interleave a line. Cancellation is cooperative via a shared
//! `CancellationToken`.
//!
//! Cells run on ordinary OS threads (never inside an async task), which is
//! required because `harness::run_cell` blocks on its own current-thread
//! runtime.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use tokio_util::sync::CancellationToken;

use iris_agent::harness::{self, CellSpec};

use crate::event::{CellEvent, CellId};
use crate::fixtures;
use crate::record::{CellRecord, SCHEMA_VERSION, ToolErr, TurnTokens};
use crate::spec::{Cell, RunSpec};
use crate::workloads::{self, WorkloadSpec};

/// Aggregate outcome of a run.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    /// Total cells in the matrix.
    pub total: usize,
    /// Cells that executed and produced a real record.
    pub completed: usize,
    /// Cells whose success check passed (and stayed valid).
    pub succeeded: usize,
    /// Cells that could not run (selection/fixture/provider error).
    pub failed: usize,
    /// Cells skipped because cancellation fired before they started.
    pub skipped: usize,
}

/// Internal worker → writer message.
enum Msg {
    Started {
        index: usize,
        cell: CellId,
    },
    Done {
        index: usize,
        record: Box<CellRecord>,
    },
}

/// Execute the run. Writes one JSONL line per cell to `spec.log_path` (single
/// writer) and calls `on_event` for every start/finish so a UI can render live.
/// Returns when all cells finish or cancellation drains the pool.
pub fn run(
    spec: &RunSpec,
    catalog: &[WorkloadSpec],
    cancel: &CancellationToken,
    mut on_event: impl FnMut(CellEvent),
) -> std::io::Result<Summary> {
    let cells = Arc::new(spec.expand());
    let catalog_map: Arc<HashMap<String, WorkloadSpec>> = Arc::new(
        catalog
            .iter()
            .map(|w| (w.name.to_string(), w.clone()))
            .collect(),
    );
    let config_cwd = Arc::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let allow_skip = spec.allow_skip_permissions;
    let next = Arc::new(AtomicUsize::new(0));
    let workers = spec.effective_concurrency();

    let mut log = File::create(&spec.log_path)?;

    let (tx, rx) = mpsc::channel::<Msg>();
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let cells = Arc::clone(&cells);
        let catalog_map = Arc::clone(&catalog_map);
        let config_cwd = Arc::clone(&config_cwd);
        let next = Arc::clone(&next);
        let cancel = cancel.clone();
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= cells.len() || cancel.is_cancelled() {
                    break;
                }
                let cell = cells[i].clone();
                let cid = CellId {
                    model: cell.model.clone(),
                    workload: cell.workload.clone(),
                    arm: cell.arm.label().to_string(),
                    run: cell.run,
                };
                if tx
                    .send(Msg::Started {
                        index: cell.index,
                        cell: cid,
                    })
                    .is_err()
                {
                    break;
                }
                let record = match catalog_map.get(&cell.workload) {
                    Some(wl) => execute_cell(&cell, wl, &config_cwd, allow_skip, &cancel),
                    None => CellRecord::error(
                        &cell.model,
                        &cell.workload,
                        cell.arm.label(),
                        cell.run,
                        "unknown workload",
                    ),
                };
                if tx
                    .send(Msg::Done {
                        index: cell.index,
                        record: Box::new(record),
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
    // The writer loop (this thread) is the ONLY log writer. Drop the local
    // sender so `rx` closes once every worker exits.
    drop(tx);

    let mut summary = Summary {
        total: cells.len(),
        ..Summary::default()
    };
    for msg in rx {
        match msg {
            Msg::Started { index, cell } => {
                on_event(CellEvent::Started { index, cell });
            }
            Msg::Done { index, record } => {
                writeln!(
                    log,
                    "{}",
                    serde_json::to_string(&record).unwrap_or_default()
                )?;
                let is_error = record.kind == "real_cell_error";
                if is_error {
                    summary.failed += 1;
                    let reason = record.error.clone().unwrap_or_default();
                    let cell = CellId {
                        model: record.model.clone(),
                        workload: record.workload.clone(),
                        arm: record.arm.clone(),
                        run: record.run,
                    };
                    on_event(CellEvent::Failed {
                        index,
                        cell,
                        reason,
                    });
                } else {
                    summary.completed += 1;
                    if record.success {
                        summary.succeeded += 1;
                    }
                    on_event(CellEvent::Finished { index, record });
                }
            }
        }
    }
    log.flush()?;
    for h in handles {
        let _ = h.join();
    }
    summary.skipped = summary
        .total
        .saturating_sub(summary.completed + summary.failed);
    Ok(summary)
}

/// Run one cell: resolve the model, materialize the fixture, execute the agent
/// turn via the harness, then score it. Every failure path returns an error
/// record so the log captures ALL outcomes.
fn execute_cell(
    cell: &Cell,
    wl: &WorkloadSpec,
    config_cwd: &Path,
    allow_skip: bool,
    cancel: &CancellationToken,
) -> CellRecord {
    let arm_label = cell.arm.label();
    let reasoning = cell.reasoning.clone().unwrap_or_default();

    if wl.skip_permissions && !allow_skip {
        return CellRecord::error(
            &cell.model,
            &cell.workload,
            arm_label,
            cell.run,
            "skip-permissions workload not acknowledged (bash disabled)",
        );
    }

    let selection =
        match harness::selection_for_spec(config_cwd, &cell.model, cell.reasoning.as_deref()) {
            Ok(sel) => sel,
            Err(e) => {
                return CellRecord::error(
                    &cell.model,
                    &cell.workload,
                    arm_label,
                    cell.run,
                    &format!("select: {e}"),
                );
            }
        };

    let workspace = match fixtures::materialize(wl.fixture_id) {
        Ok(ws) => ws,
        Err(e) => {
            return CellRecord::error(
                &cell.model,
                &cell.workload,
                arm_label,
                cell.run,
                &format!("fixture {}: {e}", wl.fixture_id),
            );
        }
    };
    if let Some(build) = wl.build {
        build(&workspace.path);
    }

    let cell_spec = CellSpec {
        workspace: &workspace.path,
        prompt: wl.prompt,
        arm: cell.arm,
        skip_permissions: wl.skip_permissions,
        selection: &selection,
        cancel,
    };
    let obs = match harness::run_cell(&cell_spec) {
        Ok(obs) => obs,
        Err(e) => {
            return CellRecord::error(&cell.model, &cell.workload, arm_label, cell.run, &e);
        }
    };

    let mut outcome = (wl.check)(&workspace.path, &obs.final_text);
    let valid =
        workloads::enforce_failing_then_passing_bash(wl, &mut outcome, &obs.bash_exit_codes);

    let tokens_per_turn = if obs.turns == 0 {
        0.0
    } else {
        obs.input_tokens as f64 / obs.turns as f64
    };
    let tool_calls_total: u32 = obs.tool_counts.values().sum();

    CellRecord {
        schema_version: SCHEMA_VERSION,
        kind: "real_cell".to_string(),
        valid,
        model: cell.model.clone(),
        workload: cell.workload.clone(),
        arm: arm_label.to_string(),
        reduce_output: cell.arm.reduce(),
        reasoning,
        run: cell.run,
        success: outcome.success,
        detail: outcome.detail,
        turns: obs.turns,
        input_tokens: obs.input_tokens,
        output_tokens: obs.output_tokens,
        reasoning_tokens: obs.reasoning_tokens,
        cache_read_tokens: obs.cache_read_tokens,
        total_tokens: obs.total_tokens,
        tokens_per_turn,
        tool_calls_total,
        tool_counts: obs.tool_counts,
        handles_stored: obs.handles_stored,
        approvals: obs.approvals_consulted,
        dangerous_approvals: obs.dangerous_approvals,
        tool_sequence: obs.tool_sequence,
        tool_errors: obs
            .tool_errors
            .into_iter()
            .map(|(name, message)| ToolErr { name, message })
            .collect(),
        tool_result_bytes: obs.tool_result_bytes,
        tool_result_bytes_by_tool: obs.tool_result_bytes_by_tool,
        bash_exit_codes: obs.bash_exit_codes,
        per_turn: obs
            .per_turn
            .into_iter()
            .map(|(input, output)| TurnTokens { input, output })
            .collect(),
        error: None,
    }
}
