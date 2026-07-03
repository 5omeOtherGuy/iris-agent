//! Shared hardened git subprocess helper (issue #262, ADR-0028).
//!
//! Every git spawn in the dirty-tree safety layer goes through here so the
//! subprocess is non-interactive and side-effect-free by construction: no
//! terminal/credential prompts, no SSH host-key prompts, no LFS smudge, and no
//! optional index lock. A hung `git fetch` credential prompt or an LFS network
//! fetch inside a safety check would be a worse failure than the check itself.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

/// Run `git <args>` in `workspace` with the hardened, non-interactive
/// environment. `--no-optional-locks` is prepended so a read-only status/ls
/// check never contends for or leaves an index lock. Returns the raw output
/// (caller inspects the exit status) so callers that tolerate a non-zero exit
/// (e.g. the git-detection probe) can branch on it.
pub(super) fn git(workspace: &Path, args: &[&str]) -> Result<Output> {
    hardened(workspace, args, &[])
        .output()
        .context("failed to spawn git subprocess")
}

/// Build a hardened, non-interactive `git` command in `workspace` with the given
/// extra environment variables. The shared choke point so every git spawn -- the
/// #262 read-only probes and the #263 checkpoint plumbing (`GIT_INDEX_FILE`
/// against a temporary index, never the user's) -- inherits the same
/// prompt-free, side-effect-free environment.
fn hardened(workspace: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(workspace)
        // Never block on a credential/terminal prompt.
        .env("GIT_TERMINAL_PROMPT", "0")
        // Never block on an SSH host-key/passphrase prompt.
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
        // Never trigger an LFS network smudge while reading working-tree state.
        .env("GIT_LFS_SKIP_SMUDGE", "1");
    for (key, value) in env {
        command.env(key, value);
    }
    command
}

/// Run a git command with extra environment and raw `stdin` bytes, returning its
/// stdout on a zero exit. Used by the checkpoint plumbing: `hash-object --stdin`
/// (blob content) and `update-index --index-info` (tree entries) against a
/// temporary `GIT_INDEX_FILE`. `stdin` bytes (not `String`) so binary blob
/// content and non-UTF-8 index-info paths pass through verbatim.
pub(super) fn git_io(
    workspace: &Path,
    args: &[&str],
    env: &[(&str, &OsStr)],
    stdin: &[u8],
) -> Result<Vec<u8>> {
    let mut child = hardened(workspace, args, env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn git subprocess")?;
    // Take the pipe and write in this scope so it drops (EOF) before we wait,
    // avoiding a deadlock when git blocks reading a large stdin.
    child
        .stdin
        .take()
        .context("git stdin pipe missing")?
        .write_all(stdin)
        .context("failed to write git stdin")?;
    let output = child.wait_with_output().context("failed to wait for git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Run a git command with extra environment (no stdin), returning stdout on a
/// zero exit. The env-carrying analogue of [`git_stdout`], for checkpoint
/// commands that read/write against a temporary `GIT_INDEX_FILE` (`write-tree`,
/// `read-tree`).
pub(super) fn git_env_stdout(
    workspace: &Path,
    args: &[&str],
    env: &[(&str, &OsStr)],
) -> Result<Vec<u8>> {
    let output = hardened(workspace, args, env)
        .output()
        .context("failed to spawn git subprocess")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Run a git command and return its stdout as bytes, erroring on a non-zero
/// exit. Bytes (not `String`) because `-z` porcelain output is NUL-delimited and
/// paths are not guaranteed UTF-8.
pub(super) fn git_stdout(workspace: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = git(workspace, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Whether `workspace` is inside a git working tree. Uses the hardened helper
/// and treats any error or a non-`true` answer as "not a git working tree" so a
/// missing `git` binary or a bare/again-degraded repo falls back cleanly to the
/// degraded (non-git) mode rather than failing the run.
pub(super) fn is_git_worktree(workspace: &Path) -> bool {
    match git(workspace, &["rev-parse", "--is-inside-work-tree"]) {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim() == "true"
        }
        _ => false,
    }
}
