//! Engine → observer event stream. The engine emits these as cells start and
//! finish so a live TUI (or a headless printer) can render progress without
//! knowing anything about the worker pool.

use crate::record::CellRecord;

/// Identity of one cell in the matrix.
#[derive(Clone, Debug)]
pub struct CellId {
    pub model: String,
    pub workload: String,
    pub arm: String,
    pub run: usize,
}

/// A progress event for one cell. `index` is the cell's position in the
/// expanded matrix (0-based), stable across the run.
#[derive(Clone, Debug)]
pub enum CellEvent {
    /// A worker picked up this cell and started executing it.
    Started { index: usize, cell: CellId },
    /// The cell completed (successfully executed; check result is inside the
    /// record's `success`/`valid`).
    Finished {
        index: usize,
        record: Box<CellRecord>,
    },
    /// The cell could not run (selection/build/provider error). The record is
    /// an error row already written to the log.
    Failed {
        index: usize,
        cell: CellId,
        reason: String,
    },
}

impl CellEvent {
    /// The matrix index this event refers to.
    pub fn index(&self) -> usize {
        match self {
            CellEvent::Started { index, .. }
            | CellEvent::Finished { index, .. }
            | CellEvent::Failed { index, .. } => *index,
        }
    }
}
