//! Shared text and I/O-size helpers.
//!
//! BOM detection, line-ending normalization/restoration, head/tail output
//! truncation, atomic file replacement, and the file-size limits shared by the
//! file tools.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand::RngCore;

/// Default output cap: keep at most the first/last 2000 lines.
pub(super) const DEFAULT_MAX_LINES: usize = 2000;
/// Default output cap: keep at most the first/last 1MB of bytes.
pub(super) const DEFAULT_MAX_BYTES: usize = 1_000_000; // 1MB
/// Largest file that `read`/`edit` will load into memory.
pub(super) const READ_TOOL_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// Largest content that `write`/`edit` will write to disk.
pub(super) const WRITE_TOOL_MAX_BYTES: usize = 100 * 1024 * 1024;

pub(super) fn strip_bom(s: &str) -> (&str, bool) {
    s.strip_prefix('\u{FEFF}')
        .map_or((s, false), |stripped| (stripped, true))
}

pub(super) fn detect_line_ending(content: &str) -> &'static str {
    let bytes = content.as_bytes();
    for (idx, &b) in bytes.iter().enumerate() {
        match b {
            b'\r' => {
                return if bytes.get(idx + 1) == Some(&b'\n') {
                    "\r\n"
                } else {
                    "\r"
                };
            }
            b'\n' => return "\n",
            _ => {}
        }
    }
    "\n"
}

pub(super) fn normalize_to_lf(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            out.push('\n');
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub(super) fn restore_line_endings(text: &str, ending: &str) -> String {
    match ending {
        "\r\n" => text.replace('\n', "\r\n"),
        "\r" => text.replace('\n', "\r"),
        _ => text.to_string(),
    }
}

/// Atomically replace `dest` with `bytes`.
///
/// Writes to a uniquely named temp file in `dest`'s parent directory, fsyncs it,
/// then renames it over `dest` in a single filesystem operation. If `dest`
/// already exists, its Unix permissions are copied onto the replacement (owner
/// and group are not preserved, and any hardlinks to the old inode are
/// detached). The temp file is removed if any step before the rename fails.
///
/// Caller contract: `dest`'s parent directory must already exist (callers
/// resolve paths through `path::resolve_for_write` / `resolve_existing` and, for
/// `write`, `create_dir_all` the parent first).
///
/// Atomicity relies on `rename(2)` replacing the destination in one step on the
/// same filesystem; the temp file is a sibling of `dest`, so that holds. Windows
/// `fs::rename` does not atomically replace an existing file (out of scope).
///
/// After the rename the parent directory is fsynced (best effort) so the swap
/// itself is durable across a crash, not just the file contents.
pub(super) fn atomic_write(dest: &Path, bytes: &[u8]) -> Result<()> {
    let parent = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let file_name = dest
        .file_name()
        .context("atomic_write: destination has no file name")?;
    // Build the temp name from OsString so non-UTF-8 filenames are preserved
    // exactly (a lossy String conversion could collide or corrupt the name).
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(file_name);
    temp_name.push(format!(".iris-tmp-{:016x}", rand::rng().next_u64()));
    let temp_path = parent.join(temp_name);

    // Read the destination's permissions up front so the temp file can be
    // locked down at creation time (avoiding a window where its contents are
    // world-readable under the default umask) and given the exact final mode
    // after the swap.
    let existing_perms = fs::metadata(dest).ok().map(|meta| meta.permissions());

    let mut guard = TempGuard::new(temp_path.clone());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        if let Some(perms) = &existing_perms {
            options.mode(perms.mode());
        }
    }
    let mut file = options
        .open(&temp_path)
        .with_context(|| format!("failed to create temp file {}", temp_path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write temp file {}", temp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to fsync temp file {}", temp_path.display()))?;
    drop(file);

    // Set the exact destination mode (the creation mode above is masked by the
    // umask, so this restores any bits the umask stripped).
    if let Some(perms) = existing_perms {
        fs::set_permissions(&temp_path, perms)
            .with_context(|| format!("failed to copy permissions to {}", temp_path.display()))?;
    }

    fs::rename(&temp_path, dest)
        .with_context(|| format!("failed to replace {}", dest.display()))?;
    guard.disarm();

    // Best effort: fsync the directory so the rename is durable across a crash.
    // Not all platforms/filesystems support directory fsync, so ignore errors.
    if let Ok(dir) = fs::File::open(&parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// RAII guard that deletes a temp file on drop unless [`disarm`](Self::disarm)
/// is called once the file has been renamed into place.
struct TempGuard {
    path: Option<PathBuf>,
}

impl TempGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Keep the first `max_lines` lines and first `max_bytes` bytes.
pub(super) fn truncate_head(
    text: &str,
    max_lines: usize,
    max_bytes: usize,
) -> (String, bool, usize) {
    let mut truncated = false;
    let mut dropped = 0;
    let lines: Vec<&str> = text.split('\n').collect();
    let mut kept = lines.clone();
    if kept.len() > max_lines {
        dropped = kept.len() - max_lines;
        kept.truncate(max_lines);
        truncated = true;
    }
    let mut out = kept.join("\n");
    if out.len() > max_bytes {
        let mut cut = max_bytes;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        truncated = true;
    }
    (out, truncated, dropped)
}

/// Keep the last `max_lines` lines and last `max_bytes` bytes.
pub(super) fn truncate_tail(
    text: &str,
    max_lines: usize,
    max_bytes: usize,
) -> (String, bool, usize) {
    let mut truncated = false;
    let mut dropped = 0;
    let lines: Vec<&str> = text.split('\n').collect();
    let kept = if lines.len() > max_lines {
        dropped = lines.len() - max_lines;
        truncated = true;
        lines[lines.len() - max_lines..].to_vec()
    } else {
        lines
    };
    let mut out = kept.join("\n");
    if out.len() > max_bytes {
        let mut start = out.len() - max_bytes;
        while start < out.len() && !out.is_char_boundary(start) {
            start += 1;
        }
        out = out[start..].to_string();
        truncated = true;
    }
    (out, truncated, dropped)
}

#[cfg(test)]
mod tests {
    use super::atomic_write;
    use crate::tools::test_support::temp_dir;
    use std::fs;

    #[test]
    fn atomic_write_creates_new_file() {
        let dir = temp_dir();
        let dest = dir.path.join("new.txt");
        atomic_write(&dest, b"hello").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello");
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = temp_dir();
        let dest = dir.path.join("f.txt");
        fs::write(&dest, b"old content here").unwrap();
        atomic_write(&dest, b"new").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_unix_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let dest = dir.path.join("script.sh");
        fs::write(&dest, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).unwrap();
        atomic_write(&dest, b"#!/bin/sh\necho hi\n").unwrap();
        let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn atomic_write_leaves_no_temp_on_success() {
        let dir = temp_dir();
        let dest = dir.path.join("f.txt");
        atomic_write(&dest, b"data").unwrap();
        let entries: Vec<String> = fs::read_dir(&dir.path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["f.txt".to_string()]);
    }

    #[test]
    fn atomic_write_cleans_up_on_failure() {
        let dir = temp_dir();
        // dest is a directory: renaming a regular temp file over it fails after
        // the temp file is created, exercising the cleanup-on-failure path.
        let dest = dir.path.join("subdir");
        fs::create_dir(&dest).unwrap();

        let err = atomic_write(&dest, b"data").unwrap_err();

        assert!(err.to_string().contains("failed to replace"));
        assert!(dest.is_dir());
        let leftovers: Vec<String> = fs::read_dir(&dir.path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("iris-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }
}
