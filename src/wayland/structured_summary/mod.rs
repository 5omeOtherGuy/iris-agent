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
//! - [`extraction`]: the provider-neutral response-side extraction step that
//!   pulls the structured JSON out of a provider's already-parsed
//!   `AssistantTurn` (native visible text, or the single forced
//!   `emit_compaction_summary` tool call) and reuses [`validate`] on it.
//! - [`durable_text`]: the deterministic durable-summary text renderer.
//!
//! Everything here is a pure function: no provider requests, no OAuth/auth
//! material, no session-log mutation, and no `append_compaction` behavior.
//! `schema`/`validate`/`extraction` back the provider request plumbing in
//! `mimir::providers::openai_codex_responses` and
//! `mimir::providers::anthropic_messages`; `input_renderer`/`extraction`/
//! `durable_text` back the `wayland::compaction` fallback-ladder wiring
//! (issue #475's summarizer-wiring slice). `SummaryValidationError` and the
//! direct `parse_compaction_summary`/`parse_compaction_summary_value`
//! validators are reached only from this module's own tests and from
//! `extraction` internally -- callers outside this module go through
//! `extract_native_summary`/`extract_forced_tool_summary`, which already
//! validate.

mod durable_text;
mod extraction;
mod input_renderer;
mod schema;
mod validate;

pub(crate) use durable_text::render_durable_summary;
pub(crate) use input_renderer::{CompactInputRange, render_compact_input};

// `SummaryExtractionError` itself is named only by this module's own tests
// and by the provider adapters' `#[cfg(test)]` extraction tests, so a
// non-test build of the lib target sees this re-export as unused.
#[allow(unused_imports)]
pub(crate) use extraction::{
    SummaryExtractionError, VIRTUAL_TOOL_NAME, extract_forced_tool_summary, extract_native_summary,
};
pub(crate) use schema::canonical_compaction_schema;
#[allow(unused_imports)]
pub(crate) use validate::{
    SummaryValidationError, parse_compaction_summary, parse_compaction_summary_value,
};
