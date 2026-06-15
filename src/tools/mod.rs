//! Native built-in tool implementations.
//!
//! These are workspace-scoped, synchronous ports of the eight built-in tools
//! that pi_agent_rust exposes from its own `src/tools.rs`:
//! `read`, `bash`, `edit`, `write`, `grep`, `find`, `ls`, and `hashline_edit`.
//!
//! Fidelity notes:
//! - The model-facing contract (tool name, description, and JSON Schema) is
//!   copied verbatim from pi so the wire surface matches.
//! - Behavior is reimplemented for Iris's synchronous, std-only runtime rather
//!   than pi's async runtime. `grep` shells out to `ripgrep` (`rg`) and `find`
//!   shells out to `fd`/`fdfind`, exactly like pi, and report the same
//!   "not available" guidance when those binaries are missing.
//! - `hashline_edit` and `read`'s `hashline` option reproduce pi's content-hash
//!   tag algorithm (xxh32 over the whitespace-stripped line, encoded with the
//!   `NIBBLE_STR` alphabet) so tags round-trip between the two tools.
//!
//! Nexus owns workspace-path enforcement: every tool resolves the requested
//! path against the canonicalized workspace root and refuses to escape it
//! (including via `..` and symlinks). See [`path`].
//!
//! Module layout:
//! - [`path`], [`text`]: shared path-resolution and text/I/O-size helpers.
//! - [`hashline`]: content-hash tags plus the `hashline_edit` tool.
//! - One module per remaining tool: [`read`], [`bash`], [`edit`], [`write`],
//!   [`grep`], [`find`], [`ls`].

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};
use serde_json::{Value, json};

mod bash;
mod edit;
mod find;
mod grep;
mod hashline;
mod ls;
mod path;
mod read;
mod text;
mod write;

#[cfg(test)]
pub(crate) use read::read_file;

/// Execute a tool call by name, returning the textual tool result.
///
/// Argument-parsing error messages are preserved where existing tests depend
/// on them (`read tool arguments must include path`).
pub(crate) fn dispatch(workspace: &Path, name: &str, args: &Value) -> Result<String> {
    let _span = tracing::debug_span!("tool_dispatch", tool = name).entered();
    let root = path::workspace_root(workspace)?;
    match name {
        "read" => read::execute(&root, args),
        "bash" => bash::execute(&root, args),
        "edit" => edit::execute(&root, args),
        "write" => write::execute(&root, args),
        "grep" => grep::execute(&root, args),
        "find" => find::execute(&root, args),
        "ls" => ls::execute(&root, args),
        "hashline_edit" => hashline::execute(&root, args),
        other => bail!("unknown tool: {other}"),
    }
}

/// Policy taxonomy for a built-in tool, single-sourcing the read-only vs
/// mutating split that drives both approval and (later) display grouping.
///
/// This is a *policy* taxonomy (approval + future read-only grouping). Display
/// verb/path matching stays local to `tool_display`, so the two concerns can
/// diverge without churning this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolClass {
    /// read, grep, find, ls — no approval; future Codex-style "exploring" set.
    ReadOnly,
    /// write, edit, hashline_edit — mutating, summarized by path.
    MutatingFile,
    /// bash — mutating, summarized by command.
    Bash,
    /// unknown / future tools.
    Other,
}

impl ToolClass {
    pub(crate) fn is_mutating(self) -> bool {
        matches!(self, ToolClass::MutatingFile | ToolClass::Bash)
    }
}

/// Classify a tool by name into its policy [`ToolClass`].
pub(crate) fn classify(name: &str) -> ToolClass {
    match name {
        "read" | "grep" | "find" | "ls" => ToolClass::ReadOnly,
        "write" | "edit" | "hashline_edit" => ToolClass::MutatingFile,
        "bash" => ToolClass::Bash,
        _ => ToolClass::Other,
    }
}

/// Nexus-owned safety policy: which built-in tools mutate the workspace and
/// therefore require user approval before execution.
///
/// Derived from [`classify`] so the read-only vs mutating split has a single
/// home and cannot drift when a tool is added.
pub(crate) fn requires_approval(name: &str) -> bool {
    classify(name).is_mutating()
}

/// Optional pre-approval diff preview for mutating tools.
pub(crate) fn diff_preview(workspace: &Path, name: &str, args: &Value) -> Option<String> {
    let root = match path::workspace_root(workspace) {
        Ok(root) => root,
        Err(error) => return Some(format!("diff unavailable: {error:#}")),
    };
    match name {
        "write" => render_preview(write::preview(&root, args)),
        "edit" => render_preview(edit::preview(&root, args)),
        "hashline_edit" => render_preview(hashline::preview(&root, args)),
        "bash" => Some("diff unavailable: bash commands do not have a file diff".to_string()),
        _ => None,
    }
}

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
        Preview::Available { path, old, new } => Some(unified_diff(&path, &old, &new)),
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

/// JSON tool declarations advertised to the provider, one per built-in tool.
///
/// Names, descriptions, and parameter schemas are copied verbatim from pi.
pub(crate) fn tool_definitions() -> Vec<Value> {
    [
        ("read", read::DESCRIPTION, read::parameters()),
        ("bash", bash::DESCRIPTION, bash::parameters()),
        ("edit", edit::DESCRIPTION, edit::parameters()),
        ("write", write::DESCRIPTION, write::parameters()),
        ("grep", grep::DESCRIPTION, grep::parameters()),
        ("find", find::DESCRIPTION, find::parameters()),
        ("ls", ls::DESCRIPTION, ls::parameters()),
        (
            "hashline_edit",
            hashline::DESCRIPTION,
            hashline::parameters(),
        ),
    ]
    .into_iter()
    .map(|(name, description, parameters)| {
        json!({
            "type": "function",
            "name": name,
            "description": description,
            "parameters": parameters,
        })
    })
    .collect()
}

/// Locate the first available external binary from `candidates`, returning a
/// `'static` name suitable for `Command::new`. Shared by `grep` and `find`.
fn find_binary(candidates: &[&str]) -> Option<&'static str> {
    for &name in candidates {
        if Command::new(name)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            // Return a 'static str matching the candidate.
            return match name {
                "rg" => Some("rg"),
                "ripgrep" => Some("ripgrep"),
                "fd" => Some("fd"),
                "fdfind" => Some("fdfind"),
                _ => None,
            };
        }
    }
    None
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
    use super::test_support::temp_dir;
    use super::{diff_preview, dispatch, tool_definitions};
    use serde_json::json;
    use std::fs;

    #[test]
    fn dispatch_unknown_tool_errors() {
        let dir = temp_dir();
        let err = dispatch(&dir.path, "nope", &json!({}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown tool: nope"));
    }

    #[test]
    fn classify_maps_tools_to_policy_class() {
        use super::ToolClass;
        for name in ["read", "grep", "find", "ls"] {
            assert_eq!(super::classify(name), ToolClass::ReadOnly, "{name}");
        }
        for name in ["write", "edit", "hashline_edit"] {
            assert_eq!(super::classify(name), ToolClass::MutatingFile, "{name}");
        }
        assert_eq!(super::classify("bash"), ToolClass::Bash);
        assert_eq!(super::classify("nope"), ToolClass::Other);
        // is_mutating matches the requires_approval set exactly.
        for name in ["write", "edit", "bash", "hashline_edit"] {
            assert!(super::classify(name).is_mutating(), "{name}");
        }
        for name in ["read", "grep", "find", "ls", "nope"] {
            assert!(!super::classify(name).is_mutating(), "{name}");
        }
    }

    #[test]
    fn requires_approval_gates_only_mutating_tools() {
        for name in ["write", "edit", "bash", "hashline_edit"] {
            assert!(super::requires_approval(name), "{name} should be gated");
        }
        for name in ["read", "grep", "find", "ls"] {
            assert!(
                !super::requires_approval(name),
                "{name} should not be gated"
            );
        }
    }

    #[test]
    fn tool_definitions_cover_all_eight() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
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
                "hashline_edit"
            ]
        );
    }

    #[test]
    fn diff_preview_renders_write_diff() {
        let dir = temp_dir();
        fs::write(dir.path.join("note.txt"), "old\n").unwrap();

        let diff = diff_preview(
            &dir.path,
            "write",
            &json!({ "path": "note.txt", "content": "new\n" }),
        )
        .unwrap();

        assert!(diff.contains("--- a/note.txt"));
        assert!(diff.contains("+++ b/note.txt"));
        assert!(diff.contains("-old"));
        assert!(diff.contains("+new"));
    }

    #[test]
    fn diff_preview_skips_malformed_mutating_args() {
        let dir = temp_dir();

        assert!(diff_preview(&dir.path, "write", &json!({ "path": "note.txt" })).is_none());
    }

    #[test]
    fn diff_preview_reports_unavailable_for_well_formed_failed_edit() {
        let dir = temp_dir();

        let preview = diff_preview(
            &dir.path,
            "edit",
            &json!({ "path": "missing.txt", "oldText": "old", "newText": "new" }),
        )
        .unwrap();

        assert!(preview.contains("diff unavailable"));
    }
}
