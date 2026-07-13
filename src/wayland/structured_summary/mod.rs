//! Typed structured-output compaction summaries (issue #475, ADR-0061).
//!
//! This module owns the provider-neutral `CompactionSummary` contract and its
//! surrounding pure functions:
//!
//! - [`schema`]: the canonical `CompactionSummary` type and its provider-safe
//!   JSON Schema.
//! - [`input_renderer`]: the deterministic parent-owned renderer from a
//!   planned compaction range to compact `F/U/A/R/TC/TR` line-oriented text.
//! - [`validate`]: local validation that parses provider JSON output into a
//!   `CompactionSummary`, rejecting anything outside the canonical shape.
//! - [`durable_text`]: the deterministic durable-summary text renderer.
//!
//! Everything here is a pure function: no provider requests, no OAuth/auth
//! material, no session-log mutation, and no `append_compaction` behavior.
//! Provider request plumbing (native structured output + forced-tool
//! fallback) and wiring this into the background summarizer path are later
//! #475 slices; nothing in the crate calls into this module yet, hence the
//! blanket `dead_code`/`unused_imports` allowances below (same pattern as
//! `wayland::git_safety::ledger`'s seam fields).

#[allow(dead_code)]
mod durable_text;
#[allow(dead_code)]
mod input_renderer;
#[allow(dead_code)]
mod schema;
#[allow(dead_code)]
mod validate;

// Re-exported for the later provider-plumbing/wiring slices (#475); nothing
// in the crate consumes these yet, so the re-exports themselves are
// unused-within-crate until then.
#[allow(unused_imports)]
pub(crate) use durable_text::render_durable_summary;
#[allow(unused_imports)]
pub(crate) use input_renderer::{CompactInputRange, render_compact_input};
#[allow(unused_imports)]
pub(crate) use schema::{CompactionSummary, canonical_compaction_schema};
#[allow(unused_imports)]
pub(crate) use validate::{
    SummaryValidationError, parse_compaction_summary, parse_compaction_summary_value,
};
