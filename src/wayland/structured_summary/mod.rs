//! Typed structured-output compaction summaries (issue #475, ADR-0061).
//!
//! This module owns the provider-neutral `CompactionSummary` contract and its
//! surrounding pure functions:
//!
//! - [`schema`]: the canonical `CompactionSummary` type and its provider-safe
//!   JSON Schema.
//! - [`input_renderer`]: the deterministic parent-owned renderer from a
//!   planned compaction range to compact `F/U/A/R/TC/TR` line-oriented text.
//!
//! Everything here is a pure function: no provider requests, no OAuth/auth
//! material, no session-log mutation, and no `append_compaction` behavior.
//! Local validation and durable-text rendering land in the next commit of
//! this slice; provider request plumbing (native structured output +
//! forced-tool fallback) and wiring this into the background summarizer path
//! are later #475 slices. Nothing in the crate calls into this module yet,
//! hence the blanket `dead_code`/`unused_imports` allowances below (same
//! pattern as `wayland::git_safety::ledger`'s seam fields).

#[allow(dead_code)]
mod input_renderer;
#[allow(dead_code)]
mod schema;

// Re-exported for the later provider-plumbing/wiring slices (#475); nothing
// in the crate consumes these yet, so the re-exports themselves are
// unused-within-crate until then.
#[allow(unused_imports)]
pub(crate) use input_renderer::{CompactInputRange, render_compact_input};
#[allow(unused_imports)]
pub(crate) use schema::{CompactionSummary, canonical_compaction_schema};
