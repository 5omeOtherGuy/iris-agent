//! iris-bench: benchmark control + analysis for the Iris agent's token-per-task
//! harness. Drives real cells only through the `iris_agent::harness` façade,
//! adds a bounded parallel engine + single-writer JSONL log, a live TUI, and an
//! offline analysis/HTML report.

pub mod analysis;
pub mod cli;
pub mod engine;
pub mod event;
pub mod fixtures;
pub mod record;
pub mod report;
pub mod spec;
pub mod style;
pub mod tui;
pub mod workloads;
