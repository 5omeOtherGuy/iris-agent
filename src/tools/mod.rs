//! Native built-in tool implementations.
//!
//! These are workspace-scoped, synchronous ports of the seven built-in tools
//! that pi_agent_rust exposes from its own `src/tools.rs`:
//! `read`, `bash`, `edit`, `write`, `grep`, `find`, and `ls`.
//!
//! Fidelity notes:
//! - The model-facing contract (tool name, description, and JSON Schema) is
//!   copied verbatim from pi so the wire surface matches.
//! - Behavior is reimplemented for Iris's synchronous, std-only runtime rather
//!   than pi's async runtime. `grep` shells out to `ripgrep` (`rg`) and `find`
//!   shells out to `fd`/`fdfind`, exactly like pi, and report the same
//!   "not available" guidance when those binaries are missing.
//! - `edit` follows Claude Code's exact-string contract
//!   (`file_path`/`old_string`/`new_string`/`replace_all`).
//!
//! Nexus owns workspace-path enforcement: every tool resolves the requested
//! path against the canonicalized workspace root and refuses to escape it
//! (including via `..` and symlinks). See [`path`].
//!
//! Module layout:
//! - [`path`], [`text`]: shared path-resolution and text/I/O-size helpers.
//! - One module per tool: [`read`], [`bash`], [`edit`], [`write`],
//!   [`grep`], [`find`], [`ls`].

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

mod bash;
mod edit;
mod find;
mod grep;
mod ls;
mod observe;
mod path;
mod read;
mod text;
mod write;

pub(crate) use observe::ObservedFiles;

#[cfg(test)]
pub(crate) use read::read_file;

/// Structured result of a successful tool call: the model-facing text plus
/// optional structured metadata. The metadata object is the seam that lets
/// large outputs become handle-backed later without changing call sites
/// (Milestone 2 gate); tools that have nothing structured to report use
/// [`ToolOutput::text`] and the metadata is simply omitted from the wire.
#[derive(Debug)]
pub(crate) struct ToolOutput {
    pub(crate) content: String,
    pub(crate) metadata: Map<String, Value>,
}

impl ToolOutput {
    /// A text-only result with no structured metadata.
    pub(crate) fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            metadata: Map::new(),
        }
    }

    /// Attach one metadata field, builder-style.
    pub(crate) fn with(mut self, key: &str, value: Value) -> Self {
        self.metadata.insert(key.to_string(), value);
        self
    }
}

/// Execute a tool call by name, returning the structured tool result.
///
/// Argument-parsing error messages are preserved where existing tests depend
/// on them (`read tool arguments must include path`).
pub(crate) fn dispatch(
    workspace: &Path,
    name: &str,
    args: &Value,
    observed: &mut ObservedFiles,
) -> Result<ToolOutput> {
    let _span = tracing::debug_span!("tool_dispatch", tool = name).entered();
    let root = path::workspace_root(workspace)?;
    match name {
        "read" => read::execute(&root, args, observed),
        "bash" => bash::execute(&root, args),
        "edit" => edit::execute(&root, args, observed),
        "write" => write::execute(&root, args, observed),
        "grep" => grep::execute(&root, args),
        "find" => find::execute(&root, args),
        "ls" => ls::execute(&root, args),
        other => bail!("unknown tool: {other}"),
    }
}

/// Nexus-owned safety policy: which built-in tools mutate the workspace and
/// therefore require user approval before execution.
pub(crate) fn requires_approval(name: &str) -> bool {
    matches!(name, "write" | "edit" | "bash")
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
        "bash" => None,
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
        let err = dispatch(
            &dir.path,
            "nope",
            &json!({}),
            &mut super::ObservedFiles::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown tool: nope"));
    }

    #[test]
    fn requires_approval_gates_only_mutating_tools() {
        for name in ["write", "edit", "bash"] {
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
    fn tool_definitions_cover_all_seven() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec!["read", "bash", "edit", "write", "grep", "find", "ls"]
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
            &json!({ "file_path": "missing.txt", "old_string": "old", "new_string": "new" }),
        )
        .unwrap();

        assert!(preview.contains("diff unavailable"));
    }
}
