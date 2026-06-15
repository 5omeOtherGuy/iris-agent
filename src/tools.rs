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
//! (including via `..` and symlinks).

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use xxhash_rust::xxh32::xxh32;

// ============================================================================
// Constants (mirror pi_agent_rust src/tools.rs)
// ============================================================================

const DEFAULT_MAX_LINES: usize = 2000;
const DEFAULT_MAX_BYTES: usize = 1_000_000; // 1MB
const GREP_MAX_LINE_LENGTH: usize = 500;
const DEFAULT_GREP_LIMIT: usize = 100;
const DEFAULT_FIND_LIMIT: usize = 1000;
const DEFAULT_LS_LIMIT: usize = 500;
const LS_SCAN_HARD_LIMIT: usize = 20_000;
const READ_TOOL_MAX_BYTES: u64 = 100 * 1024 * 1024;
const WRITE_TOOL_MAX_BYTES: usize = 100 * 1024 * 1024;
const DEFAULT_BASH_TIMEOUT_SECS: u64 = 120;
// Cap on how long we wait for the output reader threads to observe EOF after the
// shell has exited or been killed. A backgrounded process that escapes the
// shell's process group (via setsid/double-fork) can keep the pipes open; rather
// than block indefinitely we return whatever was captured within this window.
const BASH_DRAIN_TIMEOUT_SECS: u64 = 5;

/// Hashline encoding alphabet (16 letters, one per nibble), copied from pi.
const NIBBLE_STR: &[u8; 16] = b"ZPMQVRWSNKTXJBYH";

// ============================================================================
// Dispatch + provider tool declarations
// ============================================================================

/// Execute a tool call by name, returning the textual tool result.
///
/// Argument-parsing error messages are preserved where existing tests depend
/// on them (`read tool arguments must include path`).
pub(crate) fn dispatch(workspace: &Path, name: &str, args: &Value) -> Result<String> {
    let root = workspace_root(workspace)?;
    match name {
        "read" => {
            let input: ReadInput = serde_json::from_value(args.clone())
                .context("read tool arguments must include path")?;
            read(&root, &input)
        }
        "bash" => {
            let input: BashInput = serde_json::from_value(args.clone())
                .context("bash tool arguments must include command")?;
            bash(&root, &input)
        }
        "edit" => {
            let input: EditInput = serde_json::from_value(args.clone())
                .context("edit tool arguments must include path, oldText, newText")?;
            edit(&root, &input)
        }
        "write" => {
            let input: WriteInput = serde_json::from_value(args.clone())
                .context("write tool arguments must include path and content")?;
            write_file(&root, &input)
        }
        "grep" => {
            let input: GrepInput = serde_json::from_value(args.clone())
                .context("grep tool arguments must include pattern")?;
            grep(&root, &input)
        }
        "find" => {
            let input: FindInput = serde_json::from_value(args.clone())
                .context("find tool arguments must include pattern")?;
            find(&root, &input)
        }
        "ls" => {
            let input: LsInput =
                serde_json::from_value(args.clone()).context("ls tool arguments are invalid")?;
            ls(&root, &input)
        }
        "hashline_edit" => {
            let input: HashlineEditInput = serde_json::from_value(args.clone())
                .context("hashline_edit tool arguments must include path and edits")?;
            hashline_edit(&root, &input)
        }
        other => bail!("unknown tool: {other}"),
    }
}

/// JSON tool declarations advertised to the provider, one per built-in tool.
///
/// Names, descriptions, and parameter schemas are copied verbatim from pi.
pub(crate) fn tool_definitions() -> Vec<Value> {
    [
        ("read", READ_DESCRIPTION, read_parameters()),
        ("bash", BASH_DESCRIPTION, bash_parameters()),
        ("edit", EDIT_DESCRIPTION, edit_parameters()),
        ("write", WRITE_DESCRIPTION, write_parameters()),
        ("grep", GREP_DESCRIPTION, grep_parameters()),
        ("find", FIND_DESCRIPTION, find_parameters()),
        ("ls", LS_DESCRIPTION, ls_parameters()),
        ("hashline_edit", HASHLINE_DESCRIPTION, hashline_parameters()),
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

// ============================================================================
// Workspace path resolution (enforcement point)
// ============================================================================

fn workspace_root(workspace: &Path) -> Result<PathBuf> {
    workspace
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace {}", workspace.display()))
}

fn join_request(root: &Path, requested: &str) -> PathBuf {
    let candidate = Path::new(requested);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    }
}

/// Lexically normalize `.` and `..` without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve a path that must already exist, confined to the workspace.
fn resolve_existing(root: &Path, requested: &str) -> Result<PathBuf> {
    let candidate = lexical_normalize(&join_request(root, requested));
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve path {requested}"))?;
    if !resolved.starts_with(root) {
        bail!("path escapes workspace: {requested}");
    }
    Ok(resolved)
}

/// Resolve a path for create/overwrite, confined to the workspace. The path
/// need not exist, but no existing ancestor may symlink outside the workspace.
fn resolve_for_write(root: &Path, requested: &str) -> Result<PathBuf> {
    let candidate = lexical_normalize(&join_request(root, requested));
    if !candidate.starts_with(root) {
        bail!("path escapes workspace: {requested}");
    }
    let mut ancestor = candidate.as_path();
    loop {
        if ancestor.exists() {
            let canonical = ancestor
                .canonicalize()
                .with_context(|| format!("failed to resolve path {requested}"))?;
            if !canonical.starts_with(root) {
                bail!("path escapes workspace: {requested}");
            }
            break;
        }
        match ancestor.parent() {
            Some(parent) => ancestor = parent,
            None => break,
        }
    }
    Ok(candidate)
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

// ============================================================================
// read
// ============================================================================

const READ_DESCRIPTION: &str = "Read the contents of a text file. Output is truncated to 2000 lines or 1MB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.";

fn read_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
            "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
            "limit": { "type": "integer", "description": "Maximum number of lines to read" },
            "hashline": { "type": "boolean", "description": "When true, output each line as N#AB:content where N is the line number and AB is a content hash. Use with hashline_edit tool for precise edits." }
        },
        "required": ["path"]
    })
}

#[derive(Debug, Deserialize)]
struct ReadInput {
    path: String,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    hashline: bool,
}

fn read(root: &Path, input: &ReadInput) -> Result<String> {
    if matches!(input.limit, Some(limit) if limit <= 0) {
        bail!("`limit` must be greater than 0");
    }
    if matches!(input.offset, Some(offset) if offset < 0) {
        bail!("`offset` must be non-negative");
    }

    let resolved = resolve_existing(root, &input.path)?;
    let metadata =
        fs::metadata(&resolved).with_context(|| format!("failed to stat {}", input.path))?;
    if !metadata.is_file() {
        bail!("path {} is not a regular file", input.path);
    }
    if metadata.len() > READ_TOOL_MAX_BYTES {
        bail!(
            "file is too large ({} bytes). Max allowed is {READ_TOOL_MAX_BYTES} bytes.",
            metadata.len()
        );
    }

    let bytes = fs::read(&resolved).with_context(|| format!("failed to read {}", input.path))?;
    let content = String::from_utf8_lossy(&bytes);

    let mut lines: Vec<&str> = content.split('\n').collect();
    // A trailing newline produces a final empty element that is not a real line.
    if content.ends_with('\n') {
        lines.pop();
    }
    let total_lines = lines.len();
    if total_lines == 0 {
        return Ok(String::new());
    }

    let offset = input.offset.unwrap_or(1).max(1) as usize;
    let start = offset - 1;
    if start >= total_lines {
        bail!("offset {offset} is beyond end of file ({total_lines} lines total)");
    }
    let limit = input.limit.map_or(DEFAULT_MAX_LINES, |l| l as usize).max(1);
    let mut end = (start + limit).min(total_lines);

    let width = end.to_string().len().max(1);
    let mut rendered: Vec<String> = Vec::new();
    let mut byte_count = 0usize;
    let mut byte_capped = false;
    for (offset_in_window, idx) in (start..end).enumerate() {
        let line = lines[idx].strip_suffix('\r').unwrap_or(lines[idx]);
        let formatted = if input.hashline {
            format!("{}:{line}", format_hashline_tag(idx, line))
        } else {
            format!("{:>width$}\u{2192}{line}", idx + 1)
        };
        byte_count += formatted.len() + 1;
        if byte_count > DEFAULT_MAX_BYTES && offset_in_window > 0 {
            end = idx;
            byte_capped = true;
            break;
        }
        rendered.push(formatted);
    }

    let mut out = rendered.join("\n");
    if end < total_lines {
        let next_offset = end + 1;
        if byte_capped {
            out.push_str(&format!(
                "\n\n[Showing lines {}-{end} of {total_lines} (1MB limit). Use offset={next_offset} to continue.]",
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
    Ok(out)
}

/// Convenience entry used by integration tests: read with default options.
#[cfg(test)]
pub(crate) fn read_file(workspace: &Path, path: &str) -> Result<String> {
    let root = workspace_root(workspace)?;
    read(
        &root,
        &ReadInput {
            path: path.to_string(),
            offset: None,
            limit: None,
            hashline: false,
        },
    )
}

// ============================================================================
// bash
// ============================================================================

const BASH_DESCRIPTION: &str = "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 1MB (whichever is hit first). If truncated, full output is saved to a temp file. `timeout` defaults to 120 seconds; set `timeout: 0` to disable.";

fn bash_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "Bash command to execute" },
            "timeout": { "type": "integer", "description": "Timeout in seconds (default 120; set 0 to disable)" }
        },
        "required": ["command"]
    })
}

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

fn bash(root: &Path, input: &BashInput) -> Result<String> {
    if input.command.trim().is_empty() {
        bail!("bash command must not be empty");
    }
    let timeout_secs = input.timeout.unwrap_or(DEFAULT_BASH_TIMEOUT_SECS);
    let timeout = (timeout_secs > 0).then(|| Duration::from_secs(timeout_secs));

    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(&input.command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Run the shell in its own process group so a timeout can terminate the
    // whole group (including backgrounded children that keep the output pipes
    // open), not just the shell leader. With process_group(0) the child's PGID
    // equals its PID.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().context("failed to spawn shell")?;

    // Drain both pipes on dedicated threads so a full pipe buffer cannot
    // deadlock the wait loop. Each thread reports its captured bytes over a
    // channel so the collection below can apply a bounded deadline instead of
    // join()-ing forever if a process keeps a pipe open (see the drain below).
    let mut stdout = child.stdout.take().context("missing bash stdout")?;
    let mut stderr = child.stderr.take().context("missing bash stderr")?;
    let (tx, rx) = std::sync::mpsc::channel::<(BashStream, Vec<u8>)>();
    let stdout_tx = tx.clone();
    std::thread::spawn(move || pump_pipe(&mut stdout, BashStream::Stdout, &stdout_tx));
    std::thread::spawn(move || pump_pipe(&mut stderr, BashStream::Stderr, &tx));

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().context("failed to wait for shell")? {
            Some(status) => break Some(status),
            None => {
                if let Some(timeout) = timeout
                    && start.elapsed() >= timeout
                {
                    // Kill the whole process group so backgrounded children
                    // holding the output pipes are terminated too, which lets
                    // the reader threads observe EOF and the drain below return.
                    kill_process_group(&mut child);
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };

    // Accumulate chunks from both streams until the pump threads finish (the
    // channel disconnects) or the drain deadline passes. A process that escaped
    // the shell's group (setsid/double-fork) can keep a pipe open after the
    // shell exits; rather than block on it forever we return the output
    // captured so far. The streaming pump means already-written output is
    // delivered even when a later holder keeps the pipe open.
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let drain_deadline = Instant::now() + Duration::from_secs(BASH_DRAIN_TIMEOUT_SECS);
    loop {
        let remaining = drain_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok((BashStream::Stdout, chunk)) => stdout_bytes.extend_from_slice(&chunk),
            Ok((BashStream::Stderr, chunk)) => stderr_bytes.extend_from_slice(&chunk),
            Err(_) => break,
        }
    }

    let mut combined = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr_text = String::from_utf8_lossy(&stderr_bytes);
    if !stderr_text.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr_text);
    }

    let (truncated_body, truncated, dropped_lines) =
        truncate_tail(&combined, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);

    let mut out = if truncated_body.trim().is_empty() {
        "(no output)".to_string()
    } else {
        truncated_body
    };

    if truncated {
        let full_path = write_overflow_file(&combined);
        let location = full_path
            .as_ref()
            .map_or_else(|| "(unavailable)".to_string(), |p| p.display().to_string());
        out = format!(
            "[output truncated, dropped {dropped_lines} earlier line(s); full output saved to {location}]\n{out}"
        );
    }

    if timed_out {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("Command timed out after {timeout_secs} seconds"));
    } else if let Some(status) = status
        && !status.success()
    {
        let code = status.code().unwrap_or(-1);
        out.push_str(&format!("\n\nCommand exited with code {code}"));
    }

    Ok(out)
}

/// Forcefully terminate a spawned shell and the rest of its process group.
///
/// On Unix the child is spawned with `process_group(0)`, so its PGID equals its
/// PID and a single `killpg` signals every process in that group (backgrounded
/// children included). Processes that leave the group (setsid/double-fork) can
/// still escape. On other platforms we fall back to killing the leader.
#[cfg(unix)]
fn kill_process_group(child: &mut std::process::Child) {
    let Ok(pgid) = libc::pid_t::try_from(child.id()) else {
        let _ = child.kill();
        return;
    };
    // SAFETY: `killpg` is an FFI call with no Rust memory-safety invariants.
    // `pgid` is the positive id of a live child we spawned into its own process
    // group, and `SIGKILL` is a valid signal. Failures fall back to a leader
    // kill.
    let rc = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    if rc == -1 {
        let _ = child.kill();
    }
}

#[cfg(not(unix))]
fn kill_process_group(child: &mut std::process::Child) {
    let _ = child.kill();
}

#[derive(Clone, Copy)]
enum BashStream {
    Stdout,
    Stderr,
}

/// Stream a child pipe to the collector in chunks so already-written output is
/// delivered even if the pipe is later held open by an escaped process. Exits
/// on EOF, read error, or once the receiver has hung up.
fn pump_pipe(
    pipe: &mut impl Read,
    stream: BashStream,
    tx: &std::sync::mpsc::Sender<(BashStream, Vec<u8>)>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send((stream, buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        }
    }
}

fn write_overflow_file(content: &str) -> Option<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let path = std::env::temp_dir().join(format!("iris-bash-output-{nanos}.log"));
    fs::write(&path, content).ok()?;
    Some(path)
}

// ============================================================================
// edit
// ============================================================================

const EDIT_DESCRIPTION: &str = "Edit a file by replacing text. The oldText must match a unique region; matching is exact but normalizes line endings, Unicode spaces/quotes/dashes, and ignores trailing whitespace.";

fn edit_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
            "oldText": { "type": "string", "minLength": 1, "description": "Text to find and replace (must match uniquely; matching normalizes line endings, Unicode spaces/quotes/dashes, and ignores trailing whitespace)" },
            "newText": { "type": "string", "description": "New text to replace the old text with" }
        },
        "required": ["path", "oldText", "newText"]
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditInput {
    path: String,
    old_text: String,
    new_text: String,
}

fn edit(root: &Path, input: &EditInput) -> Result<String> {
    if input.new_text.len() > WRITE_TOOL_MAX_BYTES {
        bail!("new text exceeds maximum allowed size");
    }
    let resolved = resolve_existing(root, &input.path)?;
    let metadata =
        fs::metadata(&resolved).with_context(|| format!("file not found: {}", input.path))?;
    if !metadata.is_file() {
        bail!("path {} is not a regular file", input.path);
    }
    if metadata.len() > READ_TOOL_MAX_BYTES {
        bail!("file is too large to edit");
    }

    let raw = fs::read(&resolved).with_context(|| format!("failed to read {}", input.path))?;
    let raw_content = String::from_utf8(raw)
        .context("file contains invalid UTF-8 and cannot be safely edited as text")?;

    let (content_no_bom, had_bom) = strip_bom(&raw_content);
    let original_ending = detect_line_ending(content_no_bom);
    let normalized_content = normalize_to_lf(content_no_bom);
    let normalized_old = normalize_to_lf(&input.old_text);

    if normalized_old.is_empty() {
        bail!("the old text cannot be empty");
    }

    let (match_start, match_len) =
        locate_unique_match(&normalized_content, &normalized_old, &input.path)?;

    // Build the replacement against the LF-normalized content, then restore the
    // file's original line ending on write.
    let normalized_new = normalize_to_lf(&input.new_text);
    let mut new_content =
        String::with_capacity(normalized_content.len() - match_len + normalized_new.len());
    new_content.push_str(&normalized_content[..match_start]);
    new_content.push_str(&normalized_new);
    new_content.push_str(&normalized_content[match_start + match_len..]);

    if new_content == normalized_content {
        bail!(
            "no changes made to {}; replacement produced identical content",
            input.path
        );
    }

    let restored = restore_line_endings(&new_content, original_ending);
    let final_content = if had_bom {
        format!("\u{FEFF}{restored}")
    } else {
        restored
    };
    fs::write(&resolved, final_content.as_bytes())
        .with_context(|| format!("failed to write {}", input.path))?;

    Ok(format!("Successfully replaced text in {}.", input.path))
}

/// Find `needle` in `haystack`, requiring a unique match. Tries exact match
/// first, then a whitespace/Unicode-punctuation-tolerant match (mirroring pi's
/// edit normalization: Unicode spaces/quotes/dashes folded, trailing
/// whitespace per line ignored). Returns the byte range in `haystack`.
fn locate_unique_match(haystack: &str, needle: &str, path: &str) -> Result<(usize, usize)> {
    let exact = count_and_first(haystack, needle);
    match exact {
        (0, _) => {}
        (1, Some(start)) => return Ok((start, needle.len())),
        (n, _) => bail!("found {n} occurrences of the text in {path}; it must be unique"),
    }

    // Fuzzy fallback over normalized text with an offset map back to the
    // original byte positions.
    let (norm_hay, map) = normalize_for_fuzzy(haystack);
    let (norm_needle, _) = normalize_for_fuzzy(needle);
    if norm_needle.is_empty() {
        bail!("could not find the exact text in {path}; the old text must match exactly");
    }
    let (count, first) = count_and_first(&norm_hay, &norm_needle);
    match (count, first) {
        (0, _) => bail!("could not find the exact text in {path}; the old text must match exactly"),
        (1, Some(norm_start)) => {
            let norm_end = norm_start + norm_needle.len();
            let orig_start = map[norm_start];
            let orig_end = map[norm_end];
            Ok((orig_start, orig_end - orig_start))
        }
        (n, _) => bail!("found {n} occurrences of the text in {path}; it must be unique"),
    }
}

/// Count non-overlapping occurrences and report the first byte index.
fn count_and_first(haystack: &str, needle: &str) -> (usize, Option<usize>) {
    if needle.is_empty() {
        return (0, None);
    }
    let mut count = 0;
    let mut first = None;
    let mut search_from = 0;
    while let Some(rel) = haystack[search_from..].find(needle) {
        let abs = search_from + rel;
        if first.is_none() {
            first = Some(abs);
        }
        count += 1;
        search_from = abs + needle.len();
    }
    (count, first)
}

/// Build a normalized string plus a map from each normalized byte offset to the
/// originating byte offset in `input`. The map has `normalized.len() + 1`
/// entries; the final entry is the original length.
fn normalize_for_fuzzy(input: &str) -> (String, Vec<usize>) {
    // First pass: per-character normalization with origin offsets, normalizing
    // line endings to LF.
    let mut chars: Vec<(char, usize)> = Vec::new();
    let bytes = input.as_bytes();
    let mut iter = input.char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        let mapped = if ch == '\r' {
            // CRLF or lone CR collapses to a single LF.
            if iter.peek().map(|&(_, c)| c) == Some('\n') {
                iter.next();
            }
            '\n'
        } else if is_unicode_space(ch) {
            ' '
        } else if matches!(ch, '\u{2018}' | '\u{2019}') {
            '\''
        } else if matches!(ch, '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}') {
            '"'
        } else if matches!(
            ch,
            '\u{2010}'
                | '\u{2011}'
                | '\u{2012}'
                | '\u{2013}'
                | '\u{2014}'
                | '\u{2015}'
                | '\u{2212}'
        ) {
            '-'
        } else {
            ch
        };
        chars.push((mapped, idx));
    }
    let total_len = bytes.len();

    // Second pass: drop trailing whitespace before each LF and at end of input.
    let mut keep = vec![true; chars.len()];
    let mut run_is_trailing = true;
    for i in (0..chars.len()).rev() {
        let (ch, _) = chars[i];
        if ch == '\n' {
            run_is_trailing = true;
        } else if ch.is_whitespace() && run_is_trailing {
            keep[i] = false;
        } else {
            run_is_trailing = false;
        }
    }

    let mut out = String::with_capacity(input.len());
    let mut map: Vec<usize> = Vec::with_capacity(input.len() + 1);
    for (i, (ch, origin)) in chars.iter().enumerate() {
        if !keep[i] {
            continue;
        }
        let start = out.len();
        out.push(*ch);
        for _ in start..out.len() {
            map.push(*origin);
        }
    }
    map.push(total_len);
    (out, map)
}

fn is_unicode_space(c: char) -> bool {
    matches!(
        c,
        '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    ) || (c.is_whitespace() && c != '\n' && c != '\r' && c != '\t' && c != ' ')
}

// ============================================================================
// write
// ============================================================================

const WRITE_DESCRIPTION: &str = "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.";

fn write_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
            "content": { "type": "string", "description": "Content to write to the file" }
        },
        "required": ["path", "content"]
    })
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
    fs::write(&resolved, input.content.as_bytes())
        .with_context(|| format!("failed to write {}", input.path))?;
    Ok(format!(
        "Successfully wrote {} bytes to {}.",
        input.content.len(),
        input.path
    ))
}

// ============================================================================
// grep (shells out to ripgrep)
// ============================================================================

const GREP_DESCRIPTION: &str = "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 1MB (whichever is hit first). Long lines are truncated to 500 chars. Use hashline=true to get N#AB content-hash tags for use with hashline_edit.";

fn grep_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
            "path": { "type": "string", "description": "Directory or file to search (default: current directory)" },
            "glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
            "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
            "literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
            "context": { "type": "integer", "description": "Number of lines to show before and after each match (default: 0)" },
            "limit": { "type": "integer", "description": "Maximum number of matches to return (default: 100)" },
            "hashline": { "type": "boolean", "description": "When true, output each line as N#AB:content where N is the line number and AB is a content hash. Use with hashline_edit tool for precise edits." }
        },
        "required": ["pattern"]
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

fn grep(root: &Path, input: &GrepInput) -> Result<String> {
    if matches!(input.limit, Some(0)) {
        bail!("`limit` must be greater than 0");
    }
    let rg = find_binary(&["rg", "ripgrep"])
        .context("ripgrep (rg) is not available (please install ripgrep)")?;

    let search = input.path.as_deref().unwrap_or(".");
    let search_path = resolve_existing(root, search)?;
    let limit = input.limit.unwrap_or(DEFAULT_GREP_LIMIT).max(1);
    let context = input.context.unwrap_or(0);

    let mut args: Vec<String> = vec![
        "--line-number".to_string(),
        "--no-heading".to_string(),
        "--with-filename".to_string(),
        "--color=never".to_string(),
        "--hidden".to_string(),
        "--max-count".to_string(),
        limit.to_string(),
    ];
    if input.ignore_case {
        args.push("--ignore-case".to_string());
    }
    if input.literal {
        args.push("--fixed-strings".to_string());
    }
    if context > 0 {
        args.push("--context".to_string());
        args.push(context.to_string());
    }
    if let Some(glob) = &input.glob {
        args.push("--glob".to_string());
        args.push(glob.clone());
    }
    args.push("--".to_string());
    args.push(input.pattern.clone());
    args.push(search_path.to_string_lossy().to_string());

    let output = Command::new(rg)
        .args(&args)
        .current_dir(root)
        .output()
        .context("failed to run ripgrep")?;

    // ripgrep exits 1 when there are no matches; that is not an error.
    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);
        if stderr.trim().is_empty() {
            bail!("ripgrep exited with code {code}");
        }
        bail!("ripgrep failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok("No matches found".to_string());
    }

    // Rewrite absolute paths to workspace-relative and cap line length.
    let mut rendered: Vec<String> = Vec::new();
    for line in stdout.lines() {
        rendered.push(rewrite_grep_line(root, &search_path, line));
    }
    let (body, truncated, _) =
        truncate_head(&rendered.join("\n"), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut out = body;
    if truncated {
        out.push_str("\n\n[output truncated]");
    }
    Ok(out)
}

fn rewrite_grep_line(root: &Path, search_path: &Path, line: &str) -> String {
    // ripgrep lines look like `path:line:content` (match) or `path-line-content`
    // (context). Rewrite the leading absolute path to a workspace-relative one
    // and truncate over-long content.
    let search_str = search_path.to_string_lossy();
    let rest = if let Some(stripped) = line.strip_prefix(search_str.as_ref()) {
        let rel = relative_display(root, search_path);
        format!("{rel}{stripped}")
    } else {
        line.to_string()
    };
    if rest.len() > GREP_MAX_LINE_LENGTH {
        let mut cut = GREP_MAX_LINE_LENGTH;
        while cut > 0 && !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &rest[..cut])
    } else {
        rest
    }
}

// ============================================================================
// find (shells out to fd / fdfind)
// ============================================================================

const FIND_DESCRIPTION: &str = "Search for files by glob pattern. Returns matching file paths relative to the search directory. Sorted by modification time (newest first). Respects .gitignore. Output is truncated to 1000 results or 1MB (whichever is hit first).";

fn find_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'" },
            "path": { "type": "string", "description": "Directory to search in (default: current directory)" },
            "limit": { "type": "integer", "description": "Maximum number of results (default: 1000)" }
        },
        "required": ["pattern"]
    })
}

#[derive(Debug, Deserialize)]
struct FindInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn find(root: &Path, input: &FindInput) -> Result<String> {
    if matches!(input.limit, Some(0)) {
        bail!("`limit` must be greater than 0");
    }
    let fd = find_binary(&["fd", "fdfind"])
        .context("fd is not available (please install fd-find or fd)")?;

    let search = input.path.as_deref().unwrap_or(".");
    let search_path = resolve_existing(root, search)?;
    let limit = input.limit.unwrap_or(DEFAULT_FIND_LIMIT).max(1);

    let args: Vec<String> = vec![
        "--glob".to_string(),
        "--color=never".to_string(),
        "--hidden".to_string(),
        "--max-results".to_string(),
        limit.to_string(),
        "--".to_string(),
        input.pattern.clone(),
        search_path.to_string_lossy().to_string(),
    ];

    let output = Command::new(fd)
        .args(&args)
        .current_dir(root)
        .output()
        .context("failed to run fd")?;

    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);
        if stderr.trim().is_empty() {
            bail!("fd exited with code {code}");
        }
        bail!("fd failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok("No files found matching pattern".to_string());
    }

    // Collect entries with modification times so we can sort newest-first.
    let mut entries: Vec<(String, Option<SystemTime>)> = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let absolute = if Path::new(line).is_absolute() {
            PathBuf::from(line)
        } else {
            search_path.join(line)
        };
        let mut rel = relative_display(&search_path, &absolute);
        if absolute.is_dir() && !rel.ends_with('/') {
            rel.push('/');
        }
        let modified = fs::metadata(&absolute).and_then(|m| m.modified()).ok();
        entries.push((rel, modified));
    }

    entries.sort_by(|a, b| match (&a.1, &b.1) {
        (Some(at), Some(bt)) => bt
            .cmp(at)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
    });

    let listing: Vec<String> = entries.into_iter().map(|(rel, _)| rel).collect();
    let (body, truncated, _) =
        truncate_head(&listing.join("\n"), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut out = body;
    if truncated {
        out.push_str("\n\n[output truncated]");
    }
    Ok(out)
}

// ============================================================================
// ls
// ============================================================================

const LS_DESCRIPTION: &str = "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 1MB (whichever is hit first).";

fn ls_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Directory to list (default: current directory)" },
            "limit": { "type": "integer", "description": "Maximum number of entries to return (default: 500)" }
        }
    })
}

#[derive(Debug, Deserialize)]
struct LsInput {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn ls(root: &Path, input: &LsInput) -> Result<String> {
    if matches!(input.limit, Some(0)) {
        bail!("`limit` must be greater than 0");
    }
    let dir = input.path.as_deref().unwrap_or(".");
    let dir_path = resolve_existing(root, dir)?;
    if !dir_path.is_dir() {
        bail!("not a directory: {dir}");
    }
    let limit = input.limit.unwrap_or(DEFAULT_LS_LIMIT).max(1);

    let mut entries: Vec<String> = Vec::new();
    for entry in fs::read_dir(&dir_path).with_context(|| format!("cannot read directory: {dir}"))? {
        if entries.len() >= LS_SCAN_HARD_LIMIT {
            break;
        }
        let entry = entry.context("cannot read directory entry")?;
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry
            .file_type()
            .map(|ft| {
                ft.is_dir()
                    || (ft.is_symlink() && entry.metadata().map(|m| m.is_dir()).unwrap_or(false))
            })
            .unwrap_or(false);
        entries.push(if is_dir { format!("{name}/") } else { name });
    }

    if entries.is_empty() {
        return Ok("(empty directory)".to_string());
    }

    entries.sort_by_key(|name| name.to_lowercase());
    let mut truncated_entries = false;
    if entries.len() > limit {
        entries.truncate(limit);
        truncated_entries = true;
    }

    let (body, truncated_bytes, _) =
        truncate_head(&entries.join("\n"), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut out = body;
    if truncated_entries || truncated_bytes {
        out.push_str("\n\n[output truncated]");
    }
    Ok(out)
}

// ============================================================================
// hashline_edit
// ============================================================================

const HASHLINE_DESCRIPTION: &str = "Apply precise file edits using LINE#HASH tags from a prior read with hashline=true. Each edit specifies an op (replace/prepend/append), a pos anchor (\"N#AB\"), an optional end anchor for range replace, and replacement lines. Edits are validated against current file hashes and applied bottom-up to avoid index invalidation.";

fn hashline_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
            "edits": {
                "type": "array",
                "description": "Array of edit operations to apply",
                "items": {
                    "type": "object",
                    "properties": {
                        "op": { "type": "string", "enum": ["replace", "prepend", "append"], "description": "Operation type" },
                        "pos": { "type": "string", "description": "Anchor line reference in LINE#HASH format (e.g. \"5#KJ\")" },
                        "end": { "type": "string", "description": "End anchor for range replace (inclusive)" },
                        "lines": {
                            "description": "Replacement/insertion content as array of strings, single string, or null for deletion",
                            "oneOf": [
                                { "type": "array", "items": { "type": "string" } },
                                { "type": "string" },
                                { "type": "null" }
                            ]
                        }
                    },
                    "required": ["op"]
                }
            }
        },
        "required": ["path", "edits"]
    })
}

#[derive(Debug, Deserialize)]
struct HashlineEditInput {
    path: String,
    edits: Vec<HashlineOp>,
}

#[derive(Debug, Clone, Deserialize)]
struct HashlineOp {
    op: String,
    #[serde(default)]
    pos: Option<String>,
    #[serde(default)]
    end: Option<String>,
    #[serde(default)]
    lines: Option<Value>,
}

impl HashlineOp {
    fn replacement_lines(&self) -> Vec<String> {
        match &self.lines {
            None | Some(Value::Null) => vec![],
            Some(Value::String(s)) => normalize_to_lf(s).split('\n').map(String::from).collect(),
            Some(Value::Array(arr)) => arr
                .iter()
                .map(|v| match v {
                    Value::String(s) => normalize_to_lf(s),
                    other => normalize_to_lf(&other.to_string()),
                })
                .collect(),
            Some(other) => vec![normalize_to_lf(&other.to_string())],
        }
    }
}

struct ResolvedHashlineEdit {
    op: &'static str,
    start: usize,
    end: usize,
    lines: Vec<String>,
}

fn hashline_edit(root: &Path, input: &HashlineEditInput) -> Result<String> {
    if input.edits.is_empty() {
        bail!("no edits provided");
    }
    let resolved = resolve_existing(root, &input.path)?;
    let metadata =
        fs::metadata(&resolved).with_context(|| format!("file not found: {}", input.path))?;
    if !metadata.is_file() {
        bail!("path {} is not a regular file", input.path);
    }

    let raw = fs::read(&resolved).with_context(|| format!("failed to read {}", input.path))?;
    let raw_content = String::from_utf8(raw)
        .context("file contains invalid UTF-8 and cannot be safely edited as text")?;
    let (content_no_bom, had_bom) = strip_bom(&raw_content);
    let original_ending = detect_line_ending(content_no_bom);
    let normalized = normalize_to_lf(content_no_bom);
    let file_lines: Vec<&str> = normalized.split('\n').collect();

    // Validate every anchor against the current file before touching anything.
    for edit in &input.edits {
        if let Some(pos) = &edit.pos {
            validate_line_ref(pos, &file_lines, had_bom)?;
        }
        if let Some(end) = &edit.end {
            validate_line_ref(end, &file_lines, had_bom)?;
        }
    }

    let mut resolved_edits: Vec<ResolvedHashlineEdit> = Vec::new();
    for edit in &input.edits {
        let lines = edit
            .replacement_lines()
            .into_iter()
            .map(|l| strip_hashline_prefix(&l).to_string())
            .collect::<Vec<_>>();
        match edit.op.as_str() {
            "replace" => {
                let start = match &edit.pos {
                    Some(pos) => validate_line_ref(pos, &file_lines, had_bom)?,
                    None => bail!("replace operation requires a pos anchor"),
                };
                let end = match &edit.end {
                    Some(end) => validate_line_ref(end, &file_lines, had_bom)?,
                    None => start,
                };
                if end < start {
                    bail!(
                        "end anchor (line {}) is before start anchor (line {})",
                        end + 1,
                        start + 1
                    );
                }
                resolved_edits.push(ResolvedHashlineEdit {
                    op: "replace",
                    start,
                    end,
                    lines,
                });
            }
            "prepend" => {
                let idx = match &edit.pos {
                    Some(pos) => validate_line_ref(pos, &file_lines, had_bom)?,
                    None => 0,
                };
                resolved_edits.push(ResolvedHashlineEdit {
                    op: "prepend",
                    start: idx,
                    end: idx,
                    lines,
                });
            }
            "append" => {
                let idx = match &edit.pos {
                    Some(pos) => validate_line_ref(pos, &file_lines, had_bom)?,
                    None => file_lines.len().saturating_sub(1),
                };
                resolved_edits.push(ResolvedHashlineEdit {
                    op: "append",
                    start: idx,
                    end: idx,
                    lines,
                });
            }
            other => bail!("unknown op: {other:?}. Must be replace, prepend, or append."),
        }
    }

    // Apply bottom-up so earlier indices stay valid; reject overlaps.
    resolved_edits.sort_by(|a, b| {
        b.start
            .cmp(&a.start)
            .then_with(|| op_precedence(a.op).cmp(&op_precedence(b.op)))
    });
    for i in 0..resolved_edits.len() {
        for j in (i + 1)..resolved_edits.len() {
            let a = &resolved_edits[i];
            let b = &resolved_edits[j];
            if a.start <= b.end && b.start <= a.end {
                bail!(
                    "overlapping edits detected at lines {}-{} and {}-{}; combine them",
                    a.start + 1,
                    a.end + 1,
                    b.start + 1,
                    b.end + 1
                );
            }
        }
    }

    let mut lines: Vec<String> = file_lines.iter().map(|s| (*s).to_string()).collect();
    let mut any_change = false;
    for edit in &resolved_edits {
        match edit.op {
            "replace" => {
                let existing: Vec<&str> = lines[edit.start..=edit.end]
                    .iter()
                    .map(String::as_str)
                    .collect();
                let replacement: Vec<&str> = edit.lines.iter().map(String::as_str).collect();
                if existing == replacement {
                    continue;
                }
                lines.splice(edit.start..=edit.end, edit.lines.iter().cloned());
                any_change = true;
            }
            "prepend" => {
                if !edit.lines.is_empty() {
                    lines.splice(edit.start..edit.start, edit.lines.iter().cloned());
                    any_change = true;
                }
            }
            "append" if !edit.lines.is_empty() => {
                let insert_at = edit.start + 1;
                lines.splice(insert_at..insert_at, edit.lines.iter().cloned());
                any_change = true;
            }
            _ => {}
        }
    }

    if !any_change {
        bail!("no changes made to {}; all edits were no-ops", input.path);
    }

    let new_normalized = lines.join("\n");
    let restored = restore_line_endings(&new_normalized, original_ending);
    let final_content = if had_bom {
        format!("\u{FEFF}{restored}")
    } else {
        restored
    };
    fs::write(&resolved, final_content.as_bytes())
        .with_context(|| format!("failed to write {}", input.path))?;

    Ok(format!(
        "Successfully applied hashline edits to {}.",
        input.path
    ))
}

fn op_precedence(op: &str) -> u8 {
    match op {
        "replace" => 0,
        "append" => 1,
        "prepend" => 2,
        _ => 3,
    }
}

// ============================================================================
// Hashline tag algorithm (ported from pi)
// ============================================================================

/// Compute the 2-character content hash for the (0-indexed) line.
fn compute_line_hash(line_idx: usize, line: &str) -> [u8; 2] {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let significant: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    let has_alnum = significant.chars().any(|c| c.is_alphanumeric());
    let seed = if has_alnum { 0 } else { line_idx as u32 };
    let hash = xxh32(significant.as_bytes(), seed);
    let byte = (hash & 0xFF) as usize;
    [NIBBLE_STR[byte & 0x0F], NIBBLE_STR[(byte >> 4) & 0x0F]]
}

fn compute_line_hash_with_bom(line_idx: usize, line: &str, had_bom: bool) -> [u8; 2] {
    if had_bom && line_idx == 0 {
        let with_bom = format!("\u{FEFF}{line}");
        compute_line_hash(line_idx, &with_bom)
    } else {
        compute_line_hash(line_idx, line)
    }
}

fn format_hashline_tag(line_idx: usize, line: &str) -> String {
    let h = compute_line_hash(line_idx, line);
    format!("{}#{}{}", line_idx + 1, h[0] as char, h[1] as char)
}

fn format_hashline_tag_with_bom(line_idx: usize, line: &str, had_bom: bool) -> String {
    let h = compute_line_hash_with_bom(line_idx, line, had_bom);
    format!("{}#{}{}", line_idx + 1, h[0] as char, h[1] as char)
}

/// Parse a `LINE#HASH` reference, tolerating leading whitespace and diff
/// markers (`>`, `+`, `-`) plus spaces around `#`. Returns (1-indexed line, hash).
fn parse_hashline_tag(ref_str: &str) -> Result<(usize, [u8; 2])> {
    let bytes = ref_str.as_bytes();
    let mut i = 0;
    while i < bytes.len()
        && (bytes[i].is_ascii_whitespace() || matches!(bytes[i], b'>' | b'+' | b'-'))
    {
        i += 1;
    }
    let digit_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digit_start {
        bail!("invalid hashline reference: {ref_str:?}");
    }
    let line_num: usize = ref_str[digit_start..i]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid line number in {ref_str:?}"))?;
    if line_num == 0 {
        bail!("line number must be >= 1 in {ref_str:?}");
    }
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'#' {
        bail!("invalid hashline reference (missing '#'): {ref_str:?}");
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i + 2 > bytes.len() || !is_nibble(bytes[i]) || !is_nibble(bytes[i + 1]) {
        bail!("invalid hashline hash in {ref_str:?}");
    }
    Ok((line_num, [bytes[i], bytes[i + 1]]))
}

fn is_nibble(b: u8) -> bool {
    NIBBLE_STR.contains(&b)
}

/// Strip a `N#AB:` tag prefix that a model may echo into replacement content.
fn strip_hashline_prefix(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len()
        && (bytes[i].is_ascii_whitespace() || matches!(bytes[i], b'>' | b'+' | b'-'))
    {
        i += 1;
    }
    let digit_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digit_start {
        return line;
    }
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'#' {
        return line;
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i + 2 > bytes.len() || !is_nibble(bytes[i]) || !is_nibble(bytes[i + 1]) {
        return line;
    }
    i += 2;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b':' {
        &line[i + 1..]
    } else {
        line
    }
}

/// Validate a tag against the file and return its 0-indexed line.
fn validate_line_ref(ref_str: &str, file_lines: &[&str], had_bom: bool) -> Result<usize> {
    let (line_num, expected) = parse_hashline_tag(ref_str)?;
    let idx = line_num - 1;
    if idx >= file_lines.len() {
        bail!(
            "line {line_num} out of range (file has {} lines)",
            file_lines.len()
        );
    }
    let actual = compute_line_hash_with_bom(idx, file_lines[idx], had_bom);
    if actual != expected {
        let actual_tag = format_hashline_tag_with_bom(idx, file_lines[idx], had_bom);
        bail!(
            "hash mismatch at line {line_num}: expected {}#{}{}, actual is {actual_tag}; re-read the file to get current tags",
            line_num,
            expected[0] as char,
            expected[1] as char
        );
    }
    Ok(idx)
}

// ============================================================================
// Shared text helpers
// ============================================================================

fn strip_bom(s: &str) -> (&str, bool) {
    s.strip_prefix('\u{FEFF}')
        .map_or((s, false), |stripped| (stripped, true))
}

fn detect_line_ending(content: &str) -> &'static str {
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

fn normalize_to_lf(text: &str) -> String {
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

fn restore_line_endings(text: &str, ending: &str) -> String {
    match ending {
        "\r\n" => text.replace('\n', "\r\n"),
        "\r" => text.replace('\n', "\r"),
        _ => text.to_string(),
    }
}

/// Keep the first `max_lines` lines and first `max_bytes` bytes.
fn truncate_head(text: &str, max_lines: usize, max_bytes: usize) -> (String, bool, usize) {
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
fn truncate_tail(text: &str, max_lines: usize, max_bytes: usize) -> (String, bool, usize) {
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

// ============================================================================
// Binary lookup
// ============================================================================

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
mod tests {
    use super::*;

    struct TestDir {
        path: PathBuf,
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn temp_dir() -> TestDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("iris-tools-test-{nanos}"));
        fs::create_dir(&path).unwrap();
        TestDir { path }
    }

    fn root_of(dir: &TestDir) -> PathBuf {
        workspace_root(&dir.path).unwrap()
    }

    #[test]
    fn read_returns_line_numbered_content() {
        let dir = temp_dir();
        fs::write(dir.path.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let out = read_file(&dir.path, "a.txt").unwrap();
        assert!(out.contains("\u{2192}alpha"));
        assert!(out.contains("3\u{2192}gamma"));
    }

    #[test]
    fn read_offset_and_limit_window() {
        let dir = temp_dir();
        let body: String = (1..=10).map(|n| format!("line{n}\n")).collect();
        fs::write(dir.path.join("b.txt"), body).unwrap();
        let root = root_of(&dir);
        let out = read(
            &root,
            &ReadInput {
                path: "b.txt".into(),
                offset: Some(3),
                limit: Some(2),
                hashline: false,
            },
        )
        .unwrap();
        assert!(out.contains("3\u{2192}line3"));
        assert!(out.contains("4\u{2192}line4"));
        assert!(!out.contains("line5"));
        assert!(out.contains("more lines in file"));
    }

    #[test]
    fn read_rejects_escape() {
        let dir = temp_dir();
        let err = read_file(&dir.path, "../escape.txt")
            .unwrap_err()
            .to_string();
        assert!(err.contains("escapes workspace") || err.contains("failed to resolve path"));
    }

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

    #[test]
    fn edit_replaces_unique_text() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("d.txt"), "one\ntwo\nthree\n").unwrap();
        edit(
            &root,
            &EditInput {
                path: "d.txt".into(),
                old_text: "two".into(),
                new_text: "TWO".into(),
            },
        )
        .unwrap();
        let content = fs::read_to_string(dir.path.join("d.txt")).unwrap();
        assert_eq!(content, "one\nTWO\nthree\n");
    }

    #[test]
    fn edit_rejects_ambiguous_match() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("e.txt"), "dup\ndup\n").unwrap();
        let err = edit(
            &root,
            &EditInput {
                path: "e.txt".into(),
                old_text: "dup".into(),
                new_text: "x".into(),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unique"));
    }

    #[test]
    fn ls_lists_entries_with_dir_suffix() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir(dir.path.join("sub")).unwrap();
        fs::write(dir.path.join("file.txt"), "x").unwrap();
        let out = ls(
            &root,
            &LsInput {
                path: None,
                limit: None,
            },
        )
        .unwrap();
        assert!(out.contains("sub/"));
        assert!(out.contains("file.txt"));
    }

    #[test]
    fn hashline_tag_roundtrips_through_read_and_validation() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("h.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let rendered = read(
            &root,
            &ReadInput {
                path: "h.txt".into(),
                offset: None,
                limit: None,
                hashline: true,
            },
        )
        .unwrap();
        // First rendered line is `1#XY:alpha`; parse its tag and validate it.
        let first = rendered.lines().next().unwrap();
        let tag = first.split(':').next().unwrap();
        let lines = vec!["alpha", "beta", "gamma", ""];
        assert_eq!(validate_line_ref(tag, &lines, false).unwrap(), 0);
    }

    #[test]
    fn hashline_edit_replaces_anchored_line() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("k.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let file_lines = vec!["alpha", "beta", "gamma", ""];
        let tag = format_hashline_tag(1, "beta");
        hashline_edit(
            &root,
            &HashlineEditInput {
                path: "k.txt".into(),
                edits: vec![HashlineOp {
                    op: "replace".into(),
                    pos: Some(tag),
                    end: None,
                    lines: Some(Value::String("BETA".into())),
                }],
            },
        )
        .unwrap();
        let _ = file_lines;
        let content = fs::read_to_string(dir.path.join("k.txt")).unwrap();
        assert_eq!(content, "alpha\nBETA\ngamma\n");
    }

    #[test]
    fn hashline_edit_rejects_stale_tag() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("m.txt"), "alpha\nbeta\n").unwrap();
        let err = hashline_edit(
            &root,
            &HashlineEditInput {
                path: "m.txt".into(),
                edits: vec![HashlineOp {
                    op: "replace".into(),
                    pos: Some("1#ZZ".into()),
                    end: None,
                    lines: Some(Value::String("x".into())),
                }],
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("hash mismatch") || err.contains("invalid hashline"));
    }

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

    #[test]
    fn grep_finds_matches_when_rg_available() {
        if find_binary(&["rg", "ripgrep"]).is_none() {
            return;
        }
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("g.txt"), "needle here\nhaystack\n").unwrap();
        let out = grep(
            &root,
            &GrepInput {
                pattern: "needle".into(),
                path: None,
                glob: None,
                ignore_case: false,
                literal: false,
                context: None,
                limit: None,
            },
        )
        .unwrap();
        assert!(out.contains("needle here"));
        assert!(out.contains("g.txt"));
    }

    #[test]
    fn find_locates_files_when_fd_available() {
        if find_binary(&["fd", "fdfind"]).is_none() {
            return;
        }
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("target.rs"), "x").unwrap();
        let out = find(
            &root,
            &FindInput {
                pattern: "*.rs".into(),
                path: None,
                limit: None,
            },
        )
        .unwrap();
        assert!(out.contains("target.rs"));
    }

    #[test]
    fn bash_runs_command_and_captures_output() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let out = bash(
            &root,
            &BashInput {
                command: "echo hello".into(),
                timeout: None,
            },
        )
        .unwrap();
        assert!(out.contains("hello"));
    }

    #[test]
    fn bash_simple_timeout_returns() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let started = Instant::now();
        let out = bash(
            &root,
            &BashInput {
                command: "sleep 30".into(),
                timeout: Some(1),
            },
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "bash hung past timeout"
        );
        assert!(out.contains("timed out"));
    }

    #[test]
    fn bash_timeout_kills_backgrounded_pipe_holder() {
        // A backgrounded child inherits the shell's stdout pipe. If the timeout
        // path only killed the shell leader, the reader thread would block on
        // the surviving `sleep` until it exits (~30s). A process-group kill must
        // terminate it so the call returns promptly.
        let dir = temp_dir();
        let root = root_of(&dir);
        let started = Instant::now();
        let out = bash(
            &root,
            &BashInput {
                command: "sleep 30 & echo started; wait".into(),
                timeout: Some(1),
            },
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "bash hung past timeout despite backgrounded pipe holder"
        );
        assert!(out.contains("timed out"));
    }

    #[test]
    fn bash_returns_when_backgrounded_child_keeps_pipe_open() {
        // The shell exits immediately but leaves a backgrounded child holding
        // the stdout pipe. The bounded drain must let the call return rather
        // than blocking on the reader thread until the child exits (~30s).
        let dir = temp_dir();
        let root = root_of(&dir);
        let started = Instant::now();
        let out = bash(
            &root,
            &BashInput {
                command: "sleep 30 & echo done".into(),
                timeout: None,
            },
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(BASH_DRAIN_TIMEOUT_SECS + 5),
            "bash blocked on a backgrounded pipe holder"
        );
        assert!(out.contains("done"));
    }

    #[test]
    fn bash_reports_nonzero_exit() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let out = bash(
            &root,
            &BashInput {
                command: "exit 3".into(),
                timeout: None,
            },
        )
        .unwrap();
        assert!(out.contains("Command exited with code 3"));
    }
}
