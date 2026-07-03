//! Shared text and I/O-size helpers.
//!
//! BOM detection, line-ending normalization/restoration, head/tail output
//! truncation, atomic file replacement, and the file-size limits shared by the
//! file tools.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rand::Rng;
use serde_json::json;

/// Default output cap: keep at most the first/last 2000 lines.
pub(super) const DEFAULT_MAX_LINES: usize = 2000;
/// Default output cap: keep at most the first/last 50KB of bytes. Matches
/// pi-mono's truncate threshold (`harness/utils/truncate.ts` DEFAULT_MAX_BYTES =
/// 50 * 1024). This bounds what read/grep/ls/find/bash render inline; genuinely
/// large output is offloaded behind a handle by Nexus.
pub(super) const DEFAULT_MAX_BYTES: usize = 50 * 1024; // 50KB
/// Largest file that `read`/`edit` will load into memory.
pub(super) const READ_TOOL_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// Largest content that `write`/`edit` will write to disk.
pub(super) const WRITE_TOOL_MAX_BYTES: usize = 100 * 1024 * 1024;

/// Line-numbered, offset/limit-windowed rendering shared by `read` (over file
/// bytes) and `read_output` (over a stored handle's bytes), so both page a large
/// body through the identical 2000-line / 50KB caps and truncation notices
/// instead of maintaining two subtly divergent copies. `content` is the full
/// text; `offset` is 1-indexed; validation and out-of-range errors match `read`.
pub(super) struct LineWindow {
    /// Rendered, line-numbered window plus any truncation notice.
    pub(super) content: String,
    /// Total byte length of the full source text.
    pub(super) total_bytes: usize,
    /// Number of lines actually rendered in this window.
    pub(super) lines_shown: usize,
    /// Total line count of the full source text.
    pub(super) total_lines: usize,
    /// Whether the window omitted lines (line/byte cap or more content follows).
    pub(super) truncated: bool,
}

impl LineWindow {
    /// Attach this window's counts onto a [`ToolOutput`] under the same metadata
    /// keys `read` reports, so both tools expose an identical result contract.
    pub(super) fn into_output(self) -> crate::nexus::ToolOutput {
        crate::nexus::ToolOutput::text(self.content)
            .with("bytes", json!(self.total_bytes))
            .with("lines", json!(self.lines_shown))
            .with("total_lines", json!(self.total_lines))
            .with("truncated", json!(self.truncated))
    }
}

/// Validate the shared window arguments. Exposed separately so a caller that
/// does file I/O (e.g. `read`) can reject bad `offset`/`limit` *before* touching
/// the filesystem -- preserving the original pre-I/O validation order -- while
/// [`render_line_window`] still enforces the same checks for every caller from a
/// single source of truth.
pub(super) fn validate_offset_limit(offset: Option<i64>, limit: Option<i64>) -> Result<()> {
    if matches!(limit, Some(limit) if limit <= 0) {
        bail!("`limit` must be greater than 0");
    }
    if matches!(offset, Some(offset) if offset < 0) {
        bail!("`offset` must be non-negative");
    }
    Ok(())
}

/// Render `content` as line-numbered output windowed by 1-indexed `offset` and
/// `limit`, capped at [`DEFAULT_MAX_LINES`] lines / [`DEFAULT_MAX_BYTES`] bytes.
pub(super) fn render_line_window(
    content: &str,
    offset: Option<i64>,
    limit: Option<i64>,
) -> Result<LineWindow> {
    validate_offset_limit(offset, limit)?;

    let total_bytes = content.len();
    let mut lines: Vec<&str> = content.split('\n').collect();
    // A trailing newline produces a final empty element that is not a real line.
    if content.ends_with('\n') {
        lines.pop();
    }
    let total_lines = lines.len();
    if total_lines == 0 {
        return Ok(LineWindow {
            content: String::new(),
            total_bytes,
            lines_shown: 0,
            total_lines: 0,
            truncated: false,
        });
    }

    let offset = offset.unwrap_or(1).max(1) as usize;
    let start = offset - 1;
    if start >= total_lines {
        bail!("offset {offset} is beyond end of file ({total_lines} lines total)");
    }
    let limit = limit.map_or(DEFAULT_MAX_LINES, |l| l as usize).max(1);
    let mut end = (start + limit).min(total_lines);

    let width = end.to_string().len().max(1);
    let mut rendered: Vec<String> = Vec::new();
    let mut byte_count = 0usize;
    let mut byte_capped = false;
    let mut line_capped = false;
    for (offset_in_window, idx) in (start..end).enumerate() {
        let line = lines[idx].strip_suffix('\r').unwrap_or(lines[idx]);
        let formatted = format!("{:>width$}\u{2192}{line}", idx + 1);
        let (formatted, capped_line) = clamp_line_to_byte_cap(&formatted);
        byte_count += formatted.len() + 1;
        if byte_count > DEFAULT_MAX_BYTES && offset_in_window > 0 {
            end = idx;
            byte_capped = true;
            break;
        }
        rendered.push(formatted);
        if capped_line {
            end = idx + 1;
            line_capped = true;
            break;
        }
    }

    let lines_shown = end - start;
    let truncated = line_capped || end < total_lines;
    let mut out = rendered.join("\n");
    if line_capped {
        out.push_str("\n\n[Line truncated at 50KB limit.]");
    } else if end < total_lines {
        let next_offset = end + 1;
        if byte_capped {
            out.push_str(&format!(
                "\n\n[Showing lines {}-{end} of {total_lines} (50KB limit). Use offset={next_offset} to continue.]",
                start + 1
            ));
        } else {
            let remaining = total_lines - end;
            let plural = if remaining == 1 { "" } else { "s" };
            out.push_str(&format!(
                "\n\n[{remaining} more line{plural} in file. Use offset={next_offset} to continue.]"
            ));
        }
    }
    Ok(LineWindow {
        content: out,
        total_bytes,
        lines_shown,
        total_lines,
        truncated,
    })
}

fn clamp_line_to_byte_cap(line: &str) -> (String, bool) {
    if line.len() <= DEFAULT_MAX_BYTES {
        return (line.to_string(), false);
    }
    let mut cut = DEFAULT_MAX_BYTES.saturating_sub(3);
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    (format!("{}...", &line[..cut]), true)
}

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
