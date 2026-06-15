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
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("iris-tools-test-{nanos}"));
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
    use super::{dispatch, tool_definitions};
    use serde_json::json;

    #[test]
    fn dispatch_unknown_tool_errors() {
        let dir = temp_dir();
        let err = dispatch(&dir.path, "nope", &json!({}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown tool: nope"));
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
}
