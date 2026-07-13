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
//! `mimir::providers::anthropic_messages` (issue #475's provider-plumbing
//! slice). Wiring all of this into the background summarizer/fallback-ladder
//! policy (#472) is a later slice; until it lands, the summary
//! builders/extraction are reached only from tests, so the lib target still
//! sees this module as unconsumed -- hence the blanket
//! `dead_code`/`unused_imports` allowances below (same pattern as
//! `wayland::git_safety::ledger`'s seam fields).

#[allow(dead_code)]
mod durable_text;
#[allow(dead_code)]
mod extraction;
#[allow(dead_code)]
mod input_renderer;
#[allow(dead_code)]
mod schema;
#[allow(dead_code)]
mod validate;

// Still unconsumed pending the #472 background-summarizer wiring slice.
#[allow(unused_imports)]
pub(crate) use durable_text::render_durable_summary;
#[allow(unused_imports)]
pub(crate) use input_renderer::{CompactInputRange, render_compact_input};

// Consumed by the provider request plumbing added in issue #475's
// provider-plumbing slice (`mimir::providers::{openai_codex_responses,
// anthropic_messages}`); today those consumers are themselves reached only
// from tests, so the re-exports still need the allowance in the lib target.
#[allow(unused_imports)]
pub(crate) use extraction::{
    SummaryExtractionError, VIRTUAL_TOOL_NAME, extract_forced_tool_summary, extract_native_summary,
};
#[allow(unused_imports)]
pub(crate) use schema::{CompactionSummary, canonical_compaction_schema};
#[allow(unused_imports)]
pub(crate) use validate::{
    SummaryValidationError, parse_compaction_summary, parse_compaction_summary_value,
};
