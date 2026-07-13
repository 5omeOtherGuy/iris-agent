//! Typed structured-output compaction summaries (issue #475, ADR-0061).
//!
//! This module owns the provider-neutral `CompactionSummary` contract and its
//! surrounding pure functions:
//!
//! - [`schema`]: the canonical `CompactionSummary` type and its provider-safe
//!   JSON Schema.
//!
//! Everything here is a pure function: no provider requests, no OAuth/auth
//! material, no session-log mutation, and no `append_compaction` behavior.
//! The input renderer, local validation, and durable-text renderer land in
//! the following commits of this slice; provider request plumbing (native
//! structured output + forced-tool fallback) and wiring this into the
//! background summarizer path are later #475 slices. Nothing in the crate
//! calls into this module yet, hence the blanket `dead_code`/`unused_imports`
//! allowances below (same pattern as `wayland::git_safety::ledger`'s seam
//! fields).

#[allow(dead_code)]
mod schema;

// Re-exported for the later provider-plumbing/wiring slices (#475); nothing
// in the crate consumes these yet, so the re-export itself is
// unused-within-crate until then.
#[allow(unused_imports)]
pub(crate) use schema::{CompactionSummary, canonical_compaction_schema};
