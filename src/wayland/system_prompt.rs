//! Tier-2 harness-owned system prompt / context assembly.
//!
//! One owner builds the provider-visible instruction string from explicit
//! inputs, so base instructions, runtime context, and project instructions are
//! assembled in a single place instead of scattered through the provider flow.
//! Mirrors pi's `harness/system-prompt.ts` (assembly owned by the harness, not
//! the terminal UI) and Codex's `agents_md.rs` (deterministic project-doc
//! discovery), without porting either: no skills, prompt templates, plugins, or
//! per-turn `TurnContext`.
//!
//! The assembled string is fed into providers through their existing request
//! path (each provider's stored system prompt). Both fresh and resumed sessions
//! call [`assemble`] with the same workspace, so they produce identical
//! instructions -- there is no separate resume prompt path.
//!
// ponytail: assembled once per session at provider construction, which is right
// for a static root `AGENTS.md`. Issue #57 (skills / prompt templates) needs
// per-turn context (active skills, dynamic state); the upgrade path is to have
// the harness re-`assemble` and inject the prompt per turn (e.g. through the
// provider turn input) rather than baking it at construction. The single-owner
// function here is the seam that move plugs into -- no new prompt path needed.
//!
//! ## Precedence and scope
//!
//! Sources are concatenated in a fixed, deterministic order:
//!
//! 1. base Iris instructions (tool surface + guidelines),
//! 2. runtime context (the working directory),
//! 3. project instructions: the workspace-root `AGENTS.md`, when present.
//!
//! Only the **workspace-root** `AGENTS.md` is read in this slice. Nested,
//! ancestor, and user-global `AGENTS.md` discovery is deliberately deferred:
//! issue #56 names root support as the smallest safe first slice, and the fixed
//! ordering above is the seam those later sources slot into. A missing or empty
//! `AGENTS.md` is normal and contributes nothing -- it never fails assembly.
//!
//! ## Path safety
//!
//! The project doc is resolved through the workspace sandbox
//! ([`crate::tools::path::resolve_existing`]), so the only readable file is a
//! regular file at `<workspace>/AGENTS.md` that stays inside the workspace. A
//! symlink (or any path) that escapes the workspace is rejected, not read, so
//! discovery cannot exfiltrate arbitrary host files into the prompt.

use std::io::Read;
use std::path::Path;

use crate::tools::path::{resolve_existing, workspace_root};

/// Filename discovered as the project instruction source.
const AGENTS_FILENAME: &str = "AGENTS.md";

/// Upper bound on `AGENTS.md` bytes folded into the prompt. A large or runaway
/// file is truncated rather than ballooning every request.
//
// ponytail: fixed 32 KiB cap, no byte-budget config. Codex tracks a configurable
// `project_doc_max_bytes`; Iris has no setting for it yet. Make this a setting
// when a real project needs a larger or smaller instruction budget.
const MAX_AGENTS_MD_BYTES: usize = 32 * 1024;

/// Assemble the full system prompt for a turn: base instructions and runtime
/// context, with the workspace-root `AGENTS.md` appended when present. Pure in
/// `workspace` plus the on-disk `AGENTS.md`, so fresh and resumed sessions that
/// pass the same workspace assemble byte-identical instructions.
pub(crate) fn assemble(workspace: &Path) -> String {
    let mut prompt = base_instructions(workspace);
    if let Some(project) = project_instructions(workspace) {
        prompt.push_str("\n\n");
        prompt.push_str(&project);
    }
    prompt
}

/// The base, provider-neutral Iris instructions plus runtime context. Describes
/// the actual tool surface so a provider does not assume tools Iris does not
/// expose. A provider may wrap this in its own envelope (e.g. Anthropic prepends
/// the required Claude Code identity block as system block 0).
fn base_instructions(workspace: &Path) -> String {
    let prompt_cwd = workspace.display().to_string().replace('\\', "/");
    let tools = [
        "- read: Read the contents of a file by path, with optional line offset and limit.",
        "- bash: Execute a bash command in the current workspace.",
        "- edit: Edit an existing file by replacing text.",
        "- write: Create or overwrite a file, creating parent directories as needed.",
        "- grep: Search files for a regex or literal pattern.",
        "- find: Find files by glob pattern.",
        "- ls: List directory entries.",
    ]
    .join("\n");

    format!(
        "You are an expert coding assistant operating inside Iris, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\n\
Available tools:\n{tools}\n\n\
No other tools are available. Do not assume Codex CLI/native agent tools, multi_tool wrappers, subagents, or hidden parallel tool APIs exist.\n\n\
Guidelines:\n\
- Prefer read, grep, find, and ls for file inspection; use bash for shell commands and verification.\n\
- Be concise in your responses.\n\
- Show file paths clearly when working with files.\n\n\
Current working directory: {prompt_cwd}"
    )
}

/// Discover and wrap the workspace-root `AGENTS.md` for appending. `None` when
/// no readable, non-empty instruction file exists at the workspace root -- a
/// missing file is the normal case and never an error.
fn project_instructions(workspace: &Path) -> Option<String> {
    let content = read_project_doc(workspace)?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(format!(
        "The following project instructions come from {AGENTS_FILENAME} in the working directory. \
Follow them for project-specific conventions; they take precedence over the general guidelines above where they conflict.\n\n\
{trimmed}"
    ))
}

/// Read `<workspace>/AGENTS.md` if it is a regular file inside the workspace,
/// truncated at [`MAX_AGENTS_MD_BYTES`]. `None` on any miss (no file, escapes
/// the workspace, or a read error): discovery is best-effort and must never
/// fail a turn.
fn read_project_doc(workspace: &Path) -> Option<String> {
    // Resolve through the workspace sandbox so only a regular file that stays
    // inside the workspace is ever read (a symlink escaping it is rejected).
    let root = workspace_root(workspace).ok()?;
    let path = resolve_existing(&root, AGENTS_FILENAME).ok()?;
    // Open first, then check the file type from the descriptor (not the path),
    // so the regular-file guard has no TOCTOU window against a concurrent swap
    // between check and open.
    let file = std::fs::File::open(&path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    // Bounded read: cap the bytes pulled into memory so a runaway or hostile
    // huge AGENTS.md cannot OOM the process; never read the whole file first.
    let mut bytes = Vec::new();
    file.take(MAX_AGENTS_MD_BYTES as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    // Lossy decode tolerates a truncation that splits a multibyte char and any
    // stray invalid UTF-8; instruction text is best-effort, not byte-exact.
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir {
        path: PathBuf,
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn temp_dir() -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("iris-prompt-test-{nanos}-{seq}"));
        fs::create_dir(&path).unwrap();
        TempDir { path }
    }

    #[test]
    fn base_instructions_include_tool_surface_and_cwd() {
        let dir = temp_dir();
        let prompt = assemble(&dir.path);
        assert!(prompt.contains("operating inside Iris"));
        assert!(prompt.contains("Available tools:"));
        assert!(prompt.contains("Current working directory:"));
        assert!(prompt.contains(&dir.path.display().to_string()));
    }

    #[test]
    fn missing_agents_md_yields_only_base_instructions() {
        let dir = temp_dir();
        // No AGENTS.md in the workspace: assembly must not fail and must not
        // mention a project-instruction block.
        let prompt = assemble(&dir.path);
        assert!(!prompt.contains("project instructions come from"));
        assert_eq!(prompt, base_instructions(&dir.path));
    }

    #[test]
    fn root_agents_md_is_appended_after_base_instructions() {
        let dir = temp_dir();
        fs::write(dir.path.join("AGENTS.md"), "Use tabs, not spaces.").unwrap();
        let prompt = assemble(&dir.path);

        // Ordering is deterministic: base instructions first, then the project
        // block, then the AGENTS.md content.
        let base_at = prompt.find("operating inside Iris").unwrap();
        let header_at = prompt.find("project instructions come from").unwrap();
        let body_at = prompt.find("Use tabs, not spaces.").unwrap();
        assert!(
            base_at < header_at && header_at < body_at,
            "expected base < project-header < project-body order"
        );
        assert!(prompt.contains("AGENTS.md"));
    }

    #[test]
    fn empty_agents_md_contributes_nothing() {
        let dir = temp_dir();
        fs::write(dir.path.join("AGENTS.md"), "   \n\t  \n").unwrap();
        let prompt = assemble(&dir.path);
        assert!(!prompt.contains("project instructions come from"));
        assert_eq!(prompt, base_instructions(&dir.path));
    }

    #[test]
    fn fresh_and_resume_assemble_identical_instructions() {
        // `run_agent` and `resume_agent` both feed providers `assemble(&cwd)` --
        // one shared path, no separate resume builder. This pins that single
        // function's determinism, which is what makes fresh and resumed
        // instructions byte-identical for the same workspace.
        let dir = temp_dir();
        fs::write(dir.path.join("AGENTS.md"), "Project rule: be terse.").unwrap();
        let fresh = assemble(&dir.path);
        let resumed = assemble(&dir.path);
        assert_eq!(fresh, resumed);
        assert!(fresh.contains("Project rule: be terse."));
    }

    #[test]
    fn agents_md_content_is_truncated_at_the_byte_cap() {
        let dir = temp_dir();
        let big = "Q".repeat(MAX_AGENTS_MD_BYTES + 4096);
        fs::write(dir.path.join("AGENTS.md"), &big).unwrap();
        let prompt = assemble(&dir.path);
        // Content is appended last, so the prompt ends with exactly the capped
        // file bytes: at least MAX trailing 'Q', but not MAX+1 (the char before
        // the run is the '\n' of the block separator). Asserting the suffix is
        // robust even if the temp path itself contains a 'Q'.
        assert!(prompt.ends_with(&"Q".repeat(MAX_AGENTS_MD_BYTES)));
        assert!(!prompt.ends_with(&"Q".repeat(MAX_AGENTS_MD_BYTES + 1)));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_agents_md_escaping_the_workspace_is_not_read() {
        use std::os::unix::fs::symlink;

        let outside = temp_dir();
        let secret = outside.path.join("secret.txt");
        fs::write(&secret, "TOP SECRET HOST FILE").unwrap();

        let workspace = temp_dir();
        // A workspace-root AGENTS.md that symlinks to a file outside the
        // workspace must be rejected by the path sandbox, not read into the
        // prompt.
        symlink(&secret, workspace.path.join("AGENTS.md")).unwrap();

        let prompt = assemble(&workspace.path);
        assert!(
            !prompt.contains("TOP SECRET HOST FILE"),
            "an escaping symlink must not be read into the prompt"
        );
        assert!(!prompt.contains("project instructions come from"));
    }
}
