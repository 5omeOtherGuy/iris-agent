//! Native built-in tool implementations.
//!
//! These are workspace-scoped, synchronous ports of the seven built-in tools
//! that pi_agent_rust exposes from its own `src/tools.rs`:
//! `read`, `bash`, `edit`, `write`, `grep`, `find`, and `ls`, plus the
//! Iris-specific `read_output` (issue #205), which pages back an oversized tool
//! output stored out of context behind a handle (issue #61).
//!
//! Fidelity notes:
//! - The model-facing contract (tool name, description, and JSON Schema) is
//!   copied verbatim from pi so the wire surface matches.
//! - Behavior is reimplemented for Iris's synchronous, std-only runtime rather
//!   than pi's async runtime. `grep` and `find` search via the ripgrep library
//!   crates (`grep`/`ignore`/`globset`), so neither needs an external binary on
//!   PATH.
//! - `edit` follows Claude Code's exact-string contract
//!   (`file_path`/`old_string`/`new_string`/`replace_all`).
//!
//! Mutating tools require approval by default. Workspace-path enforcement is a
//! separate opt-in via `IRIS_SECURITY_OPT_IN=1`: by default tools resolve
//! requested paths but do not refuse workspace escapes. See [`path`].
//!
//! Module layout:
//! - [`path`], [`text`]: shared path-resolution and text/I/O-size helpers.
//! - One module per tool: [`read`], [`bash`], [`edit`], [`write`],
//!   [`grep`], [`find`], [`ls`].

mod bash;
mod edit;
mod find;
mod grep;
mod ls;
mod observe;
pub(crate) mod path;
mod read;
mod read_output;
mod registry;
mod text;
mod write;

// The result contract lives in Tier-1 Nexus; tools produce it and re-export it
// here so the per-tool modules can keep referring to `super::ToolOutput`.
pub(crate) use crate::nexus::ToolOutput;
pub(crate) use bash::platform_can_sandbox;
pub(crate) use observe::ObservedFiles;
pub(crate) use registry::built_in_tools;

const MAX_DIFF_PREVIEW_BYTES: usize = 1024 * 1024;

/// SHA-256 hex of `bytes`: the single content-hash convention shared by the
/// dirty-tree guard's baseline/snapshot re-hash and the mutating tools'
/// post-write confirmation hash. One implementation keeps a tool's reported
/// written-content hash directly comparable to the guard's on-disk re-hash, so
/// an approved write is attributed to Iris only when the bytes match (ADR-0028
/// TOCTOU rule); any mismatch stays protected/user-attributed.
pub(crate) fn content_hash(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Mutable per-agent state threaded into tools via [`crate::nexus::ToolEnv`]:
/// observed-file tracking for read-before-write safety plus the bash tool's
/// persistent-session registry. Owned by the `Agent` so no global mutable state
/// is needed (relocated to the harness tier in Step C).
pub(crate) struct ToolState {
    pub(crate) observed: ObservedFiles,
    /// The bash tool's persistent-session/background-job registry. Held behind
    /// an `Arc<Mutex<_>>` (not just the env's `RefCell`) so a `bash` run can be
    /// moved onto `tokio::task::spawn_blocking` while keeping the executor free
    /// to poll: the blocking task and this persistent state share ownership, so
    /// a cancelled (and thus detached, un-abortable) `spawn_blocking` still
    /// mutates the same registry rather than dropping every session/job on the
    /// floor. The bash tool is exclusive, so the lock never actually contends.
    pub(crate) bash: std::sync::Arc<std::sync::Mutex<bash::BashState>>,
}

impl ToolState {
    pub(crate) fn new() -> Self {
        Self {
            observed: ObservedFiles::new(),
            bash: std::sync::Arc::new(std::sync::Mutex::new(bash::BashState::new())),
        }
    }
}

#[cfg(test)]
pub(crate) use read::read_file;
#[cfg(test)]
pub(crate) use registry::read_output_tool;

pub(super) enum Preview {
    Available {
        path: String,
        old: String,
        new: String,
    },
    Unavailable(String),
    Malformed,
}

fn render_preview(preview: Preview) -> Option<String> {
    match preview {
        Preview::Available { path, old, new } => {
            if old.len().saturating_add(new.len()) > MAX_DIFF_PREVIEW_BYTES {
                Some("diff unavailable: preview too large".to_string())
            } else {
                Some(unified_diff(&path, &old, &new))
            }
        }
        Preview::Unavailable(reason) => Some(format!("diff unavailable: {reason}")),
        Preview::Malformed => None,
    }
}

fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let old_header = format!("a/{path}");
    let new_header = format!("b/{path}");
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .header(&old_header, &new_header)
        .to_string()
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub(crate) struct TestDir {
        pub(crate) path: PathBuf,
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    pub(crate) fn temp_dir() -> TestDir {
        // nanos alone can collide across parallel tests; a process-unique counter
        // guarantees a distinct directory per call.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("iris-tools-test-{nanos}-{seq}"));
        fs::create_dir(&path).unwrap();
        TestDir { path }
    }

    pub(crate) fn root_of(dir: &TestDir) -> PathBuf {
        super::path::workspace_root(&dir.path).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::built_in_tools;
    use super::test_support::{TestDir, temp_dir};
    use serde_json::{Value, json};
    use std::fs;

    #[test]
    fn unknown_tool_is_absent_from_the_built_in_set() {
        // The `unknown tool: <name>` error is produced by the loop's resolution
        // path (see nexus tests); the set itself simply has no such tool.
        assert!(built_in_tools().by_name("nope").is_none());
    }

    #[test]
    fn requires_approval_gates_only_mutating_tools() {
        let tools = built_in_tools();
        for name in ["write", "edit", "bash"] {
            assert!(
                tools.by_name(name).unwrap().requires_approval(),
                "{name} should be gated"
            );
        }
        for name in ["read", "grep", "find", "ls", "read_output"] {
            assert!(
                !tools.by_name(name).unwrap().requires_approval(),
                "{name} should not be gated"
            );
        }
    }

    #[test]
    fn is_destructive_flags_dangerous_bash() {
        let tools = built_in_tools();
        let bash = tools.by_name("bash").unwrap();
        for cmd in [
            "rm -rf foo",
            "mkdir x && rm x",
            "find . -delete",
            "git reset --hard",
            "sudo rmdir d",
            "echo x | dd of=/dev/sda",
        ] {
            assert!(
                bash.is_destructive(&json!({ "command": cmd })),
                "{cmd} should be destructive"
            );
        }
    }

    #[test]
    fn is_destructive_allows_benign_bash_and_other_tools() {
        let tools = built_in_tools();
        let bash = tools.by_name("bash").unwrap();
        for cmd in [
            "echo hi",
            "ls -la",
            "mkdir -p out",
            "pwd && date",
            "cat file.txt",
        ] {
            assert!(
                !bash.is_destructive(&json!({ "command": cmd })),
                "{cmd} should be benign"
            );
        }
        assert!(
            !tools
                .by_name("write")
                .unwrap()
                .is_destructive(&json!({ "path": "a", "content": "x" }))
        );
    }

    #[test]
    fn built_in_tools_cover_all_in_registration_order() {
        let tools = built_in_tools();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(
            names,
            vec![
                "read",
                "bash",
                "edit",
                "write",
                "grep",
                "find",
                "ls",
                "read_output"
            ]
        );
    }

    #[test]
    fn bash_definition_advertises_job_actions() {
        let tools = built_in_tools();
        let params = tools.by_name("bash").unwrap().parameters();
        let action_enum = params["properties"]["action"]["enum"].as_array().unwrap();
        for action in [
            "run", "reset", "close", "start", "poll", "finalize", "cancel", "list",
        ] {
            assert!(
                action_enum.contains(&json!(action)),
                "missing bash action {action}"
            );
        }
        assert!(params["properties"].get("job").is_some());
    }

    #[test]
    fn bash_definition_does_not_require_command_for_job_actions() {
        let tools = built_in_tools();
        let params = tools.by_name("bash").unwrap().parameters();
        assert!(params.get("required").is_none());
    }

    /// Resolve a built-in tool and render its diff preview against `dir`.
    fn preview(dir: &TestDir, name: &str, args: Value) -> Option<String> {
        built_in_tools()
            .by_name(name)
            .unwrap()
            .diff_preview(&dir.path, &args)
    }

    #[test]
    fn diff_preview_renders_write_diff() {
        let dir = temp_dir();
        fs::write(dir.path.join("note.txt"), "old\n").unwrap();

        let diff = preview(
            &dir,
            "write",
            json!({ "path": "note.txt", "content": "new\n" }),
        )
        .unwrap();

        assert!(diff.contains("--- a/note.txt"));
        assert!(diff.contains("+++ b/note.txt"));
        assert!(diff.contains("-old"));
        assert!(diff.contains("+new"));
    }

    #[test]
    fn diff_preview_refuses_huge_previews() {
        let dir = temp_dir();
        let diff = preview(
            &dir,
            "write",
            json!({ "path": "huge.txt", "content": "x".repeat(2 * 1024 * 1024) }),
        )
        .unwrap();

        assert_eq!(diff, "diff unavailable: preview too large");
    }

    #[test]
    fn diff_preview_absolute_write_path_has_no_double_slash() {
        let dir = temp_dir();
        let root = super::path::workspace_root(&dir.path).unwrap();
        let abs = root.join("note.txt");
        fs::write(&abs, "old\n").unwrap();

        let diff = preview(
            &dir,
            "write",
            json!({ "path": abs.to_string_lossy(), "content": "new\n" }),
        )
        .unwrap();

        assert!(!diff.contains("a//"), "double slash in header: {diff}");
        assert!(
            diff.contains("--- a/note.txt"),
            "header not relative: {diff}"
        );
        assert!(diff.contains("+++ b/note.txt"));
    }

    #[test]
    fn diff_preview_absolute_edit_path_matches_relative_write_header() {
        let dir = temp_dir();
        let root = super::path::workspace_root(&dir.path).unwrap();
        let abs = root.join("note.txt");
        fs::write(&abs, "old\n").unwrap();

        // `edit`'s schema takes an absolute `file_path`; the rendered header must
        // be the same workspace-relative path `write` produces, not `a//abs`.
        let diff = preview(
            &dir,
            "edit",
            json!({
                "file_path": abs.to_string_lossy(),
                "old_string": "old",
                "new_string": "new"
            }),
        )
        .unwrap();

        assert!(!diff.contains("a//"), "double slash in header: {diff}");
        assert!(
            diff.contains("--- a/note.txt"),
            "header not relative: {diff}"
        );
    }

    #[test]
    fn diff_preview_skips_malformed_mutating_args() {
        let dir = temp_dir();

        assert!(preview(&dir, "write", json!({ "path": "note.txt" })).is_none());
    }

    #[test]
    fn diff_preview_reports_unavailable_for_well_formed_failed_edit() {
        let dir = temp_dir();

        let preview = preview(
            &dir,
            "edit",
            json!({ "file_path": "missing.txt", "old_string": "old", "new_string": "new" }),
        )
        .unwrap();

        assert!(preview.contains("diff unavailable"));
    }
}
