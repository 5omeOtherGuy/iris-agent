//! Model-facing long-running goal tools.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::nexus::{GoalController, ToolOutput, ToolOutputStore};

pub(crate) const GET_DESCRIPTION: &str =
    "Get the current session goal, including status, objective, budgets, usage, and elapsed time.";
pub(crate) const CREATE_DESCRIPTION: &str = "Create a persistent long-running goal only when explicitly requested by the user or system/developer instructions; do not infer goals from ordinary tasks. Set a budget only when that limit was explicitly requested. Fails while a goal is unfinished. Token budgets count fresh input plus output, excluding cache reads; time budgets count active goal-turn seconds.";
pub(crate) const UPDATE_DESCRIPTION: &str = "Mark the current goal `complete` only when fully achieved, or `blocked` only after the same blocker recurs for at least three consecutive goal turns.";

pub(crate) fn empty_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

pub(crate) fn create_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "objective": {
                "type": "string",
                "description": "Goal objective. Values over 4,000 characters become a read_output attachment reference."
            },
            "token_budget": {
                "type": "integer",
                "minimum": 1,
                "description": "Optional token budget. Omit unless explicitly requested. Counts fresh input plus output, excluding cache reads."
            },
            "time_budget_seconds": {
                "type": "integer",
                "minimum": 1,
                "description": "Optional active goal-turn seconds. Omit unless explicitly requested."
            }
        },
        "required": ["objective"],
        "additionalProperties": false
    })
}

pub(crate) fn update_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": {
                "type": "string",
                "enum": ["complete", "blocked"]
            }
        },
        "required": ["status"],
        "additionalProperties": false
    })
}

pub(crate) fn get(controller: Option<&dyn GoalController>) -> Result<ToolOutput> {
    let controller = require(controller)?;
    Ok(ToolOutput::text(controller.get_goal()?.to_string()))
}

pub(crate) fn create(
    args: &Value,
    controller: Option<&dyn GoalController>,
    output_store: Option<&dyn ToolOutputStore>,
) -> Result<ToolOutput> {
    let objective = args
        .get("objective")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("create_goal requires string `objective`"))?;
    let token_budget = match args.get("token_budget") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|budget| *budget > 0)
                .ok_or_else(|| anyhow::anyhow!("create_goal `token_budget` must be positive"))?,
        ),
    };
    let time_budget_seconds = match args.get("time_budget_seconds") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().filter(|budget| *budget > 0).ok_or_else(|| {
            anyhow::anyhow!("create_goal `time_budget_seconds` must be positive")
        })?),
    };
    let controller = require(controller)?;
    let objective = crate::goal::materialize_oversized_objective(objective, output_store)?;
    Ok(ToolOutput::text(
        controller
            .create_goal(&objective, token_budget, time_budget_seconds)?
            .to_string(),
    ))
}

pub(crate) fn update(args: &Value, controller: Option<&dyn GoalController>) -> Result<ToolOutput> {
    let status = args
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("update_goal requires string `status`"))?;
    if !matches!(status, "complete" | "blocked") {
        bail!("the model may update a goal only to complete or blocked");
    }
    let controller = require(controller)?;
    Ok(ToolOutput::text(
        controller.update_goal(status)?.to_string(),
    ))
}

fn require(controller: Option<&dyn GoalController>) -> Result<&dyn GoalController> {
    controller.ok_or_else(|| {
        anyhow::anyhow!("goals require a saved session; session persistence is unavailable")
    })
}
