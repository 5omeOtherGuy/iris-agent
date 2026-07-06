//! The durable per-cell record — one JSON object per line in the run log. The
//! field layout mirrors the original `#[cfg(test)]` bench schema (v3) so the
//! analyzer and any existing tooling read the same shape. Readers MUST tolerate
//! unknown extra fields.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// JSONL schema version. Bump when fields are renamed/removed (adding is safe).
pub const SCHEMA_VERSION: u32 = 3;

/// A single (input, output) token pair for one provider turn.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct TurnTokens {
    #[serde(rename = "in")]
    pub input: u64,
    #[serde(rename = "out")]
    pub output: u64,
}

/// A tool error captured during a cell (name + truncated message).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolErr {
    pub name: String,
    pub message: String,
}

/// One benchmark cell result. `kind` discriminates a completed cell
/// (`"real_cell"`) from an unreachable/errored one (`"real_cell_error"`), so a
/// run logs ALL outcomes, never silently dropping failures.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CellRecord {
    pub schema_version: u32,
    pub kind: String,
    pub valid: bool,
    pub model: String,
    pub workload: String,
    pub arm: String,
    pub reduce_output: bool,
    pub reasoning: String,
    pub run: usize,
    pub success: bool,
    pub detail: String,
    pub turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
    pub tokens_per_turn: f64,
    pub tool_calls_total: u32,
    pub tool_counts: BTreeMap<String, u32>,
    pub handles_stored: u32,
    pub approvals: bool,
    pub dangerous_approvals: u32,
    pub tool_sequence: Vec<String>,
    pub tool_errors: Vec<ToolErr>,
    pub tool_result_bytes: u64,
    pub tool_result_bytes_by_tool: BTreeMap<String, u64>,
    pub bash_exit_codes: Vec<i32>,
    pub per_turn: Vec<TurnTokens>,
    /// Present only for `real_cell_error` rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CellRecord {
    /// Build an error/unreachable record (no metrics, marked invalid).
    pub fn error(model: &str, workload: &str, arm: &str, run: usize, reason: &str) -> Self {
        CellRecord {
            schema_version: SCHEMA_VERSION,
            kind: "real_cell_error".to_string(),
            valid: false,
            model: model.to_string(),
            workload: workload.to_string(),
            arm: arm.to_string(),
            reduce_output: false,
            reasoning: String::new(),
            run,
            success: false,
            detail: String::new(),
            turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            cache_read_tokens: 0,
            total_tokens: 0,
            tokens_per_turn: 0.0,
            tool_calls_total: 0,
            tool_counts: BTreeMap::new(),
            handles_stored: 0,
            approvals: false,
            dangerous_approvals: 0,
            tool_sequence: Vec::new(),
            tool_errors: Vec::new(),
            tool_result_bytes: 0,
            tool_result_bytes_by_tool: BTreeMap::new(),
            bash_exit_codes: Vec::new(),
            per_turn: Vec::new(),
            error: Some(reason.to_string()),
        }
    }
}
