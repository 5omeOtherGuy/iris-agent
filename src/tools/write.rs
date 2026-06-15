//! `write` — create or overwrite a file, creating parent directories.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::path::resolve_for_write;
use super::text::{WRITE_TOOL_MAX_BYTES, atomic_write};

pub(super) const DESCRIPTION: &str = "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
            "content": { "type": "string", "description": "Content to write to the file" }
        },
        "required": ["path", "content"]
    })
}

pub(super) fn execute(root: &Path, args: &Value) -> Result<String> {
    let input: WriteInput = serde_json::from_value(args.clone())
        .context("write tool arguments must include path and content")?;
    write_file(root, &input)
}

#[derive(Debug, Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

fn write_file(root: &Path, input: &WriteInput) -> Result<String> {
    if input.content.len() > WRITE_TOOL_MAX_BYTES {
        bail!("content exceeds maximum allowed size");
    }
    let resolved = resolve_for_write(root, &input.path)?;
    if let Some(parent) = resolved.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent directories for {}", input.path))?;
    }
    atomic_write(&resolved, input.content.as_bytes())
        .with_context(|| format!("failed to write {}", input.path))?;
    Ok(format!(
        "Successfully wrote {} bytes to {}.",
        input.content.len(),
        input.path
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::read::read_file;
    use crate::tools::test_support::{root_of, temp_dir};

    #[test]
    fn write_creates_parent_dirs_and_read_roundtrips() {
        let dir = temp_dir();
        let root = root_of(&dir);
        write_file(
            &root,
            &WriteInput {
                path: "nested/dir/c.txt".into(),
                content: "hello".into(),
            },
        )
        .unwrap();
        let out = read_file(&dir.path, "nested/dir/c.txt").unwrap();
        assert!(out.contains("hello"));
    }

    #[test]
    fn write_rejects_escape() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let err = write_file(
            &root,
            &WriteInput {
                path: "../evil.txt".into(),
                content: "x".into(),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("escapes workspace"));
    }
}
