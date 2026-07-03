//! Built-in tool adapters (Tier 3).
//!
//! Each struct is a thin [`Tool`] impl over the per-tool `execute`/`parameters`
//! functions plus the self-classification (`requires_approval`,
//! `is_destructive`, `is_concurrency_safe`, `diff_preview`) the core loop used to
//! compute by tool name. [`built_in_tools`] is the injection point: the CLI
//! constructs the set and passes it into the agent, so Nexus instantiates no
//! tool itself.
//!
//! The pure read-only tools (`grep`/`find`/`ls`) touch no [`ToolState`], so
//! `execute` runs their blocking body on `tokio::task::spawn_blocking` and
//! awaits the handle: they are `is_concurrency_safe` and a parallel batch runs
//! them genuinely concurrently on the blocking pool, while awaiting the handle
//! lets the loop's cancellation race abandon a cancelled call. `read` mutates
//! `state.observed` (read-before-write tracking) through the env's `!Send`
//! `RefCell`, so it cannot move off-thread and stays exclusive. Mutating/shell
//! tools (`edit`/`write`/`bash`) wrap their synchronous body in a ready future
//! and run exclusively; each borrows the shared `ToolState` only for its
//! synchronous duration, never across an `.await`.

use std::cell::RefMut;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::nexus::{Tool, ToolEnv, ToolFuture, ToolOutput, Tools};

use super::{
    Preview, ToolState, bash, edit, find, grep, ls, path, read, read_output, render_preview, write,
};

/// Construct the workspace tools the CLI injects into the agent. The order is
/// the provider-declaration order (`read, bash, edit, write, grep, find, ls`),
/// with the Iris-specific `read_output` (issue #205) appended last.
pub(crate) fn built_in_tools() -> Tools {
    Tools::new(vec![
        Box::new(ReadTool),
        Box::new(BashTool),
        Box::new(EditTool),
        Box::new(WriteTool),
        Box::new(GrepTool),
        Box::new(FindTool),
        Box::new(LsTool),
        Box::new(ReadOutputTool),
    ])
}

/// Boxed `read_output` tool for integration tests that pair it with a custom
/// tool (e.g. one that emits an oversized output) in a single [`Tools`] set.
#[cfg(test)]
pub(crate) fn read_output_tool() -> Box<dyn Tool> {
    Box::new(ReadOutputTool)
}

/// Resolve the canonicalized workspace root for an execution. Centralized here
/// (it was the first line of the old `dispatch`) so every tool enforces the
/// same path boundary.
fn root(env: &ToolEnv) -> Result<PathBuf> {
    path::workspace_root(env.workspace)
}

/// Borrow the shared tool state mutably for a synchronous tool body. Uses
/// `try_borrow_mut` so a (theoretical) overlapping borrow becomes a tool error
/// rather than a panic; tool bodies never hold this across an `.await`, so it
/// never actually contends.
fn state_mut<'e>(env: &'e ToolEnv<'_>) -> Result<RefMut<'e, ToolState>> {
    env.state
        .try_borrow_mut()
        .map_err(|_| anyhow!("tool state is busy; concurrent mutation is not allowed"))
}

/// Run a pure read-only tool body (`grep`/`find`/`ls`) on the blocking pool.
/// The body touches no [`ToolState`], so the resolved root and owned args move
/// into a `spawn_blocking` task: a parallel batch then runs genuinely
/// concurrently, and awaiting the join handle makes the future yield so the
/// loop's cancellation race can abandon a cancelled call (the orphaned walk
/// finishes on the pool and its result is discarded -- `spawn_blocking` cannot
/// be force-aborted).
fn run_off_thread(
    root: Result<PathBuf>,
    args: Value,
    label: &'static str,
    body: fn(&Path, &Value) -> Result<ToolOutput>,
) -> ToolFuture<'static> {
    Box::pin(async move {
        let root = root?;
        match tokio::task::spawn_blocking(move || body(&root, &args)).await {
            Ok(result) => result,
            Err(join_err) => Err(anyhow!("{} tool task failed: {}", label, join_err)),
        }
    })
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
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let root = root(env)?;
            let mut state = state_mut(env)?;
            read::execute(&root, args, &mut state.observed)
        })
    }
    // `read` mutates `state.observed` (read-before-write tracking) behind the
    // env's `!Send` RefCell, so it cannot run off-thread and is not
    // concurrency-safe; it takes the exclusive path (default).
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
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let root = root(env)?;
            let mut state = state_mut(env)?;
            bash::execute(&root, args, &mut state.bash, &cancel, env.output_sink)
        })
    }
    fn requires_approval(&self) -> bool {
        // Approval is independent of workspace/path confinement. Print mode
        // denies this by default, and interactive mode asks before running it.
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
    fn is_mutating(&self) -> bool {
        // A shell command may write anything: it opens the dirty-tree task and
        // is bracketed by the guard's snapshot/verify (issue #262). No static
        // path set, so `mutates_paths` stays empty and detection runs instead.
        true
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
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let root = root(env)?;
            let mut state = state_mut(env)?;
            edit::execute(&root, args, &mut state.observed)
        })
    }
    fn requires_approval(&self) -> bool {
        // Approval is independent of workspace/path confinement. Print mode
        // denies this by default, and interactive mode asks before running it.
        true
    }
    fn supports_allow_always(&self) -> bool {
        // A blanket "always" on edit would authorize arbitrary later edits to
        // any workspace file; edits stay approval-per-call until policy is
        // path/exact-call scoped (roadmap #14).
        false
    }
    fn is_mutating(&self) -> bool {
        true
    }
    fn mutates_paths(&self, args: &Value) -> Vec<PathBuf> {
        // `edit` targets its `file_path` argument. The guard normalizes it
        // against the workspace, so a relative or absolute value both resolve.
        mutated_path(args, "file_path")
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
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let root = root(env)?;
            let mut state = state_mut(env)?;
            write::execute(&root, args, &mut state.observed)
        })
    }
    fn requires_approval(&self) -> bool {
        // Approval is independent of workspace/path confinement. Print mode
        // denies this by default, and interactive mode asks before running it.
        true
    }
    fn supports_allow_always(&self) -> bool {
        // A blanket "always" on write would authorize arbitrary later writes to
        // any workspace file; writes stay approval-per-call until policy is
        // path/exact-call scoped (roadmap #14).
        false
    }
    fn is_mutating(&self) -> bool {
        true
    }
    fn mutates_paths(&self, args: &Value) -> Vec<PathBuf> {
        // `write` targets its `path` argument.
        mutated_path(args, "path")
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
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        run_off_thread(root(env), args.clone(), "grep", grep::execute)
    }
    fn is_concurrency_safe(&self) -> bool {
        true
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
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        run_off_thread(root(env), args.clone(), "find", find::execute)
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
}

struct ReadOutputTool;
impl Tool for ReadOutputTool {
    fn name(&self) -> &str {
        "read_output"
    }
    fn description(&self) -> &str {
        read_output::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        read_output::parameters()
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        // Reads back an offloaded output via the `ToolOutputStore` contract. The
        // store is a non-`'static` borrow (`env.output_store`), so this cannot
        // move the body onto `run_off_thread`'s blocking pool the way
        // `grep`/`find`/`ls` do; it does the small store read inline in the async
        // body like `read`/`edit`. It touches no `ToolState`, only the immutable
        // store, so it is still `is_concurrency_safe` and may join a parallel
        // read-only batch.
        Box::pin(async move { read_output::execute(env.output_store, args) })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
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
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        run_off_thread(root(env), args.clone(), "ls", ls::execute)
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
}

/// Extract a single mutated path from a string-valued tool argument (issue
/// #262). Returns an empty vec when the argument is missing or not a string, so
/// a malformed call is simply not dirty-gated (it fails in the tool body).
fn mutated_path(args: &Value, key: &str) -> Vec<PathBuf> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| vec![PathBuf::from(value)])
        .unwrap_or_default()
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
        .filter(|token| !token.is_empty())
        .any(|token| {
            let command = token.rsplit('/').next().unwrap_or(token);
            let command = destructive_command_basename(command);
            DANGER_TOKENS.contains(&command.as_str()) || command.starts_with("mkfs.")
        });
    if token_danger {
        return true;
    }
    // Multi-word / flag patterns a single-token scan cannot catch.
    const DANGER_PHRASES: &[&str] = &[
        "-delete",
        "git reset --hard",
        "git clean",
        // Recoverability destroyers that discard uncommitted work (ADR-0028):
        // both restore working-tree paths from the index/HEAD, wiping edits.
        "git checkout --",
        "git restore",
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

/// Normalize the command word enough for destructive-command classification:
/// path-qualified basenames (`/bin/rm`), quoted command words (`'rm'`), and
/// escaped spellings (`\rm`, `r\m`) all invoke the same shell command. This is
/// intentionally conservative; false positives cost a prompt, false negatives
/// could persist or auto-approve a destructive command.
fn destructive_command_basename(token: &str) -> String {
    token
        .chars()
        .filter(|c| !matches!(c, '\\' | '\'' | '"'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash_args(command: &str) -> Value {
        json!({ "command": command })
    }

    #[test]
    fn destructive_bash_detection_catches_path_qualified_variants() {
        for command in [
            "/bin/rm -rf target",
            "/usr/bin/dd if=/dev/zero of=file",
            "mkfs.ext4 /dev/sdz",
        ] {
            assert!(
                bash_command_is_destructive(&bash_args(command)),
                "{command} should be destructive"
            );
        }
    }

    #[test]
    fn destructive_bash_detection_catches_recoverability_destroyers() {
        // ADR-0028: commands that discard uncommitted work must re-prompt.
        for command in [
            "git checkout -- .",
            "git checkout -- src/main.rs",
            "git clean -fd",
            "git restore .",
            "git restore --staged --worktree file",
            "rm -rf target",
            "git reset --hard HEAD",
        ] {
            assert!(
                bash_command_is_destructive(&bash_args(command)),
                "{command} should be destructive"
            );
        }
    }

    #[test]
    fn destructive_bash_detection_catches_quoted_and_escaped_commands() {
        for command in [
            "\\rm -rf target",
            "r\\m -rf target",
            "'rm' -rf target",
            "\"rm\" -rf target",
            "git status; /bin/r\\m -rf target",
        ] {
            assert!(
                bash_command_is_destructive(&bash_args(command)),
                "{command} should be destructive"
            );
        }
    }
}
