//! Built-in tool adapters (Tier 3).
//!
//! Each struct is a thin [`Tool`] impl over the per-tool `execute`/`parameters`
//! functions plus the self-classification (`requires_approval`,
//! `is_destructive`, `diff_preview`) the core loop used to compute by tool name.
//! [`built_in_tools`] is the injection point: the CLI constructs the set and
//! passes it into the agent, so Nexus instantiates no tool itself.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use crate::nexus::{Tool, ToolEnv, ToolOutput, Tools};

use super::{Preview, bash, edit, find, grep, ls, path, read, render_preview, write};

/// Construct the seven workspace tools the CLI injects into the agent. The
/// order is the provider-declaration order (`read, bash, edit, write, grep,
/// find, ls`).
pub(crate) fn built_in_tools() -> Tools {
    Tools::new(vec![
        Box::new(ReadTool),
        Box::new(BashTool),
        Box::new(EditTool),
        Box::new(WriteTool),
        Box::new(GrepTool),
        Box::new(FindTool),
        Box::new(LsTool),
    ])
}

/// Resolve the canonicalized workspace root for an execution. Centralized here
/// (it was the first line of the old `dispatch`) so every tool enforces the
/// same path boundary.
fn root(env: &ToolEnv) -> Result<PathBuf> {
    path::workspace_root(env.workspace)
}

/// Render a mutating tool's preview, resolving the root from the raw workspace
/// exactly as the old `diff_preview` free function did.
fn render(workspace: &Path, preview: impl FnOnce(&Path) -> Preview) -> Option<String> {
    let root = match path::workspace_root(workspace) {
        Ok(root) => root,
        Err(error) => return Some(format!("diff unavailable: {error:#}")),
    };
    render_preview(preview(&root))
}

struct ReadTool;
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        read::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        read::parameters()
    }
    fn execute(&self, args: &Value, env: &mut ToolEnv) -> Result<ToolOutput> {
        let root = root(env)?;
        read::execute(&root, args, &mut env.state.observed)
    }
}

struct BashTool;
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        bash::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        bash::parameters()
    }
    fn execute(&self, args: &Value, env: &mut ToolEnv) -> Result<ToolOutput> {
        let root = root(env)?;
        bash::execute(&root, args, &mut env.state.bash)
    }
    fn requires_approval(&self) -> bool {
        true
    }
    fn is_destructive(&self, args: &Value) -> bool {
        bash_command_is_destructive(args)
    }
    fn supports_allow_always(&self) -> bool {
        // A blanket "always" on bash would authorize any later shell command;
        // shell stays approval-per-call.
        false
    }
}

struct EditTool;
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        edit::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        edit::parameters()
    }
    fn execute(&self, args: &Value, env: &mut ToolEnv) -> Result<ToolOutput> {
        let root = root(env)?;
        edit::execute(&root, args, &mut env.state.observed)
    }
    fn requires_approval(&self) -> bool {
        true
    }
    fn diff_preview(&self, workspace: &Path, args: &Value) -> Option<String> {
        render(workspace, |root| edit::preview(root, args))
    }
}

struct WriteTool;
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        write::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        write::parameters()
    }
    fn execute(&self, args: &Value, env: &mut ToolEnv) -> Result<ToolOutput> {
        let root = root(env)?;
        write::execute(&root, args, &mut env.state.observed)
    }
    fn requires_approval(&self) -> bool {
        true
    }
    fn diff_preview(&self, workspace: &Path, args: &Value) -> Option<String> {
        render(workspace, |root| write::preview(root, args))
    }
}

struct GrepTool;
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        grep::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        grep::parameters()
    }
    fn execute(&self, args: &Value, env: &mut ToolEnv) -> Result<ToolOutput> {
        let root = root(env)?;
        grep::execute(&root, args)
    }
}

struct FindTool;
impl Tool for FindTool {
    fn name(&self) -> &str {
        "find"
    }
    fn description(&self) -> &str {
        find::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        find::parameters()
    }
    fn execute(&self, args: &Value, env: &mut ToolEnv) -> Result<ToolOutput> {
        let root = root(env)?;
        find::execute(&root, args)
    }
}

struct LsTool;
impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        ls::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        ls::parameters()
    }
    fn execute(&self, args: &Value, env: &mut ToolEnv) -> Result<ToolOutput> {
        let root = root(env)?;
        ls::execute(&root, args)
    }
}

/// Whether a bash command performs a destructive, data-losing operation. The
/// check is deliberately conservative and biased toward flagging: a false
/// positive costs one extra prompt, a false negative could auto-run an `rm`.
fn bash_command_is_destructive(args: &Value) -> bool {
    let Some(command) = args.get("command").and_then(Value::as_str) else {
        return false;
    };
    let lower = command.to_ascii_lowercase();
    // Whole-word commands that destroy files/filesystems/devices.
    const DANGER_TOKENS: &[&str] = &[
        "rm", "rmdir", "shred", "mkfs", "dd", "truncate", "fdisk", "mkswap", "wipefs",
    ];
    let token_danger = lower
        .split(|c: char| c.is_whitespace() || matches!(c, '&' | '|' | ';' | '(' | ')' | '`'))
        .any(|token| DANGER_TOKENS.contains(&token));
    if token_danger {
        return true;
    }
    // Multi-word / flag patterns a single-token scan cannot catch.
    const DANGER_PHRASES: &[&str] = &[
        "-delete",
        "git reset --hard",
        "git clean",
        "git push --force",
        "git push -f",
        "chmod -r",
        "chown -r",
        ":(){",
        "of=/dev/",
        "> /dev/sd",
    ];
    DANGER_PHRASES.iter().any(|phrase| lower.contains(phrase))
}
