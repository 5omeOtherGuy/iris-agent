//! Assistant-message live-stream controller (issue #87).
//!
//! Replaces the previous whole-buffer streaming preview (which committed once on
//! `AssistantTextEnd` and visually "snapped", most visibly for markdown tables)
//! with a Codex-grade incremental controller:
//!
//! - [`collector`] newline-gates raw source (never commits a partial line).
//! - [`table_holdback`] finds the largest source prefix of *closed* markdown
//!   blocks that is safe to commit without a later delta reflowing an
//!   already-committed row (this holds incomplete tables/lists/code fences in
//!   the mutable tail until they are complete).
//! - [`controller`] renders the committed prefix incrementally into scrollback,
//!   keeps the remainder as a single mutable active tail, and paces the
//!   stable-line drain with the adaptive [`chunking`] policy on the loop's
//!   commit tick.
//!
//! Portions are derived from OpenAI Codex (Apache-2.0); see the per-file
//! SPDX headers, the root `NOTICE`, and `LICENSE-APACHE`.

mod chunking;
mod collector;
mod controller;
mod table_holdback;

pub(super) use controller::StreamController;
