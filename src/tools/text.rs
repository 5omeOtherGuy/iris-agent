//! Shared text and I/O-size helpers.
//!
//! BOM detection, line-ending normalization/restoration, head/tail output
//! truncation, and the file-size limits shared by the file tools.

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
