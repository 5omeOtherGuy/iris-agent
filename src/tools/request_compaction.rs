//! Model-initiated compaction request.
//!
//! The tool only sets a session-local flag. Wayland consumes it at the next
//! pair-closed governor boundary, so tool execution never rewrites the model's
//! context underneath the current provider response.

use std::sync::atomic::Ordering;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{ToolOutput, ToolState};

pub(crate) const DESCRIPTION: &str = "Schedule normal context compaction at the next safe boundary. This request does not compact immediately.";

pub(crate) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    })
}

pub(crate) fn execute(args: &Value, state: &ToolState) -> Result<ToolOutput> {
    if !args.as_object().is_some_and(serde_json::Map::is_empty) {
        bail!("request_compaction accepts no arguments");
    }
    state.compaction_requested.store(true, Ordering::SeqCst);
    Ok(ToolOutput::text(
        "Compaction is scheduled for the next safe boundary; it has not happened yet.",
    ))
}
