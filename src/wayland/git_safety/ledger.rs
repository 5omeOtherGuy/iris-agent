//! Attribution ledger (issue #262, ADR-0028).
//!
//! Every mutation observed during a task is recorded here with its path,
//! before/after content hashes, attribution, and op-log metadata (turn, tool
//! call, timestamp). The shape is deliberately op-log-modeled (jj's operation
//! log): #263 unifies this with the `refs/iris/*` checkpoint chain so ledger and
//! checkpoints are one structure. This slice records entries and exposes them;
//! it does not build git objects or offer rollback.
//!
//! Attribution rule (TOCTOU, ADR-0028): a change Iris made through an approved
//! mutating call is [`Attribution::Iris`]; any change that cannot be attributed
//! with certainty is [`Attribution::User`] and protected. The loop enforces the
//! protection (halt + restore); the ledger is the record.

use std::path::PathBuf;
use std::time::SystemTime;

/// Who a recorded change is attributed to.
//
// The ledger records these fields now so #263 (checkpoint/rollback) and #264
// (final diff) can consume them; this slice writes the op-log but does not yet
// read every field back, so the seam fields are allow(dead_code) until then.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Attribution {
    /// An approved Iris mutation (an approved `edit`/`write`, or a change a
    /// tool Iris ran made to a previously-clean file).
    Iris,
    /// A change that could not be attributed to Iris with certainty; protected.
    User,
}

/// One recorded mutation. Before/after are content hashes (`None` = absent).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(super) struct LedgerEntry {
    pub(super) path: PathBuf,
    pub(super) before: Option<String>,
    pub(super) after: Option<String>,
    pub(super) attribution: Attribution,
    /// Task-local turn/op sequence number (op-log ordering, not the provider
    /// turn id).
    pub(super) turn: u64,
    /// Originating tool-call id when known (`None` for an async-attributed
    /// change with no single owning call).
    pub(super) tool_call: Option<String>,
    pub(super) timestamp: SystemTime,
}

/// The task's ordered record of mutations. The `#263` checkpoint chain will
/// wrap this; today it is an append-only vector.
#[derive(Default)]
pub(super) struct Ledger {
    pub(super) entries: Vec<LedgerEntry>,
}

impl Ledger {
    pub(super) fn record(&mut self, entry: LedgerEntry) {
        self.entries.push(entry);
    }
}
