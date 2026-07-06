//! The run specification and matrix expansion. A `RunSpec` is the whole
//! benchmark request (what the UI/CLI collects); `expand()` turns it into the
//! ordered list of concrete cells the engine executes.

use std::path::PathBuf;

use iris_agent::harness::Arm;

/// A fully-specified benchmark run.
#[derive(Clone, Debug)]
pub struct RunSpec {
    /// `provider:model` specs to sweep.
    pub models: Vec<String>,
    /// Reasoning effort held identical across arms (e.g. `Some("low")`).
    pub reasoning: Option<String>,
    /// Workload names to run (must exist in the catalog).
    pub workloads: Vec<String>,
    /// Reduction arms to compare.
    pub arms: Vec<Arm>,
    /// Repetitions per (model, workload, arm) cell.
    pub runs: usize,
    /// Max cells executing concurrently (bounded worker pool size).
    pub concurrency: usize,
    /// JSONL run-log path (single-writer target).
    pub log_path: PathBuf,
    /// Explicit acknowledgement required to run skip-permissions (bash)
    /// workloads, which execute real shell commands in a temp workspace.
    pub allow_skip_permissions: bool,
}

/// One concrete unit of work.
#[derive(Clone, Debug)]
pub struct Cell {
    /// Position in the expanded matrix (stable across the run).
    pub index: usize,
    pub model: String,
    pub reasoning: Option<String>,
    pub workload: String,
    pub arm: Arm,
    pub run: usize,
}

impl RunSpec {
    /// Expand the matrix into ordered cells: model × workload × arm × run.
    /// Ordering groups all runs of a cell together so a partial run still has
    /// complete cells.
    pub fn expand(&self) -> Vec<Cell> {
        let mut cells = Vec::with_capacity(self.cell_count());
        let mut index = 0;
        for model in &self.models {
            for workload in &self.workloads {
                for arm in &self.arms {
                    for run in 0..self.runs {
                        cells.push(Cell {
                            index,
                            model: model.clone(),
                            reasoning: self.reasoning.clone(),
                            workload: workload.clone(),
                            arm: *arm,
                            run: run + 1,
                        });
                        index += 1;
                    }
                }
            }
        }
        cells
    }

    /// Total number of cells the matrix will produce.
    pub fn cell_count(&self) -> usize {
        self.models.len() * self.workloads.len() * self.arms.len() * self.runs
    }

    /// Effective worker-pool size: never zero, never more than the cell count.
    pub fn effective_concurrency(&self) -> usize {
        self.concurrency.max(1).min(self.cell_count().max(1))
    }
}
