//! `write` — create or overwrite a file, creating parent directories.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::path::resolve_for_write;
use super::text::{WRITE_TOOL_MAX_BYTES, atomic_write};
use super::{ObservedFiles, Preview};

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

pub(super) fn execute(
    root: &Path,
    args: &Value,
    observed: &mut ObservedFiles,
) -> Result<super::ToolOutput> {
    let input: WriteInput = serde_json::from_value(args.clone())
        .context("write tool arguments must include path and content")?;
    let message = write_file(root, &input, observed)?;
    // Report the exact bytes written so the dirty-tree guard can confirm an
    // approved write against disk (ADR-0028 TOCTOU rule); Nexus strips this key
    // before it reaches provider context.
    Ok(super::ToolOutput::text(message)
        .with("bytes_written", json!(input.content.len()))
        .with(
            crate::nexus::WRITE_CONFIRM_HASH_KEY,
            json!(super::content_hash(input.content.as_bytes())),
        ))
}

pub(super) fn preview(root: &Path, args: &Value) -> Preview {
    let input: WriteInput = match serde_json::from_value(args.clone()) {
        Ok(input) => input,
        Err(_) => return Preview::Malformed,
    };
    match write_preview(root, &input) {
        // Use the workspace-relative resolved path for the diff header so an
        // absolute `path` arg cannot produce `--- a//home/...` and write/edit
        // headers stay consistent (both relative).
        Ok((path, old, new)) => Preview::Available { path, old, new },
        Err(error) => Preview::Unavailable(format!("{error:#}")),
    }
}

#[derive(Debug, Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

fn write_file(root: &Path, input: &WriteInput, observed: &mut ObservedFiles) -> Result<String> {
    let (old, target) = prepare_write(root, input)?;
    // Overwriting an existing file requires that the agent has seen its current
    // contents; reject a blind clobber of changes made behind its back. A new
    // file (did not exist) is a blind create, which is allowed.
    if super::path::restrictions_enabled() && target.exists() {
        observed.ensure_fresh(&target, old.as_bytes())?;
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent directories for {}", input.path))?;
    }
    atomic_write(&target, input.content.as_bytes())
        .with_context(|| format!("failed to write {}", input.path))?;
    observed.observe(&target, input.content.as_bytes());
    Ok(format!(
        "Successfully wrote {} bytes to {}.",
        input.content.len(),
        input.path
    ))
}

fn write_preview(root: &Path, input: &WriteInput) -> Result<(String, String, String)> {
    let (old, target) = prepare_write(root, input)?;
    let path = super::path::relative_display(root, &target);
    Ok((path, old, input.content.clone()))
}

fn prepare_write(root: &Path, input: &WriteInput) -> Result<(String, std::path::PathBuf)> {
    if input.content.len() > WRITE_TOOL_MAX_BYTES {
        bail!("content exceeds maximum allowed size");
    }
    let resolved = resolve_for_write(root, &input.path)?;
    // If the target is an existing symlink, write through to its real target
    // (matching fs::write) instead of replacing the link with a regular file.
    // resolve_for_write already verified the link's target is in the workspace.
    let target = if resolved
        .symlink_metadata()
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
    {
        resolved
            .canonicalize()
            .with_context(|| format!("failed to resolve path {}", input.path))?
    } else {
        resolved
    };
    let old = if target.exists() {
        fs::read_to_string(&target).with_context(|| format!("failed to read {}", input.path))?
    } else {
        String::new()
    };
    Ok((old, target))
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
            &mut ObservedFiles::new(),
        )
        .unwrap();
        let out = read_file(&dir.path, "nested/dir/c.txt").unwrap();
        assert!(out.contains("hello"));
    }

    #[cfg(unix)]
    #[test]
    fn write_through_symlink_updates_target_and_keeps_link() {
        use std::fs;
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(root.join("target.txt"), "old").unwrap();
        std::os::unix::fs::symlink(root.join("target.txt"), root.join("link.txt")).unwrap();

        // Overwriting an existing target requires a prior observation.
        let mut observed = ObservedFiles::new();
        observed.observe(&root.join("link.txt"), b"old");
        write_file(
            &root,
            &WriteInput {
                path: "link.txt".into(),
                content: "new".into(),
            },
            &mut observed,
        )
        .unwrap();

        // Target was updated through the link, and the link is still a symlink.
        assert_eq!(fs::read_to_string(root.join("target.txt")).unwrap(), "new");
        assert!(
            fs::symlink_metadata(root.join("link.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
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
            &mut ObservedFiles::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("escapes workspace"));
    }

    #[test]
    fn write_to_unobserved_existing_file_is_rejected() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(root.join("out.txt"), "old").unwrap();
        let err = write_file(
            &root,
            &WriteInput {
                path: "out.txt".into(),
                content: "new".into(),
            },
            &mut ObservedFiles::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("has not been read this session"), "{err}");
        // Blind clobber refused; file untouched.
        assert_eq!(fs::read_to_string(root.join("out.txt")).unwrap(), "old");
    }

    #[test]
    fn write_to_new_file_needs_no_prior_read() {
        let dir = temp_dir();
        let root = root_of(&dir);
        write_file(
            &root,
            &WriteInput {
                path: "fresh.txt".into(),
                content: "hi".into(),
            },
            &mut ObservedFiles::new(),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(root.join("fresh.txt")).unwrap(), "hi");
    }

    #[test]
    fn write_to_stale_file_is_rejected() {
        use std::fs;
        let dir = temp_dir();
        let root = root_of(&dir);
        let path = root.join("out.txt");
        fs::write(&path, "old").unwrap();
        let mut observed = ObservedFiles::new();
        observed.observe(&path, b"old");
        // Changed on disk behind the agent's back.
        fs::write(&path, "externally changed").unwrap();
        let err = write_file(
            &root,
            &WriteInput {
                path: "out.txt".into(),
                content: "new".into(),
            },
            &mut observed,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("changed since it was last read"), "{err}");
    }
}
