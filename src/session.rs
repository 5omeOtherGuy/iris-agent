//! Session transcript persistence.
//!
//! Writes the conversation to a JSONL file so a finished session can be
//! reviewed later. The format mirrors pi's session files at the MVP level: a
//! `session` header line followed by one `message` line per entry, appended as
//! the turn progresses.
//!
//! Scope is deliberately the linear transcript only. pi's tree structure
//! (branching, `parentId` links), compaction entries, labels, and `/resume`
//! loading are later-milestone concerns and are intentionally not implemented
//! here.
//!
//! Location (mirrors pi's `~/.pi/agent/sessions/...`):
//!
//! ```text
//! <root>/<cwd-slug>/<unix-ms>_<id>.jsonl
//! ```
//!
//! where `<root>` is `IRIS_SESSION_DIR` if set, else `~/.iris/sessions`, and
//! `<cwd-slug>` is the working directory with path separators flattened.

use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::nexus::Message;

/// Current transcript format version. v1 is the linear (non-tree) layout.
const SESSION_VERSION: u32 = 1;

/// An open JSONL transcript file. Each [`append`](Self::append) writes one line
/// and flushes, so a crash leaves a valid prefix of the conversation on disk.
#[derive(Debug)]
pub(crate) struct SessionLog {
    file: File,
    path: PathBuf,
}

impl SessionLog {
    /// Create a new transcript under the resolved session root for `cwd`,
    /// writing the header line.
    pub(crate) fn create(cwd: &Path) -> Result<Self> {
        Self::create_in(&sessions_root()?, cwd)
    }

    /// Core constructor with an explicit session root, so callers and tests can
    /// persist without env or home-directory state.
    pub(crate) fn create_in(root: &Path, cwd: &Path) -> Result<Self> {
        let dir = root.join(cwd_slug(cwd));
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create session dir {}", dir.display()))?;
        let id = format!("{:08x}", rand::random::<u32>());
        let path = dir.join(format!("{}_{}.jsonl", now_ms(), id));
        let mut file = File::create(&path)
            .with_context(|| format!("failed to create session file {}", path.display()))?;
        let header = json!({
            "type": "session",
            "version": SESSION_VERSION,
            "id": id,
            "timestamp": now_ms(),
            "cwd": cwd.to_string_lossy(),
        });
        write_line(&mut file, &header)
            .with_context(|| format!("failed to write session header to {}", path.display()))?;
        Ok(Self { file, path })
    }

    /// Append one conversation message as a `message` entry line.
    pub(crate) fn append(&mut self, message: &Message) -> Result<()> {
        write_line(&mut self.file, &message_entry(message))
            .with_context(|| format!("failed to append to session {}", self.path.display()))
    }

    /// Path of the transcript file on disk.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Serialize one message into its session entry value.
fn message_entry(message: &Message) -> Value {
    let mut inner = json!({
        "role": message.role.as_str(),
        "content": message.content,
    });
    if let Some(id) = &message.tool_call_id {
        inner["toolCallId"] = json!(id);
    }
    if let Some(name) = &message.tool_name {
        inner["toolName"] = json!(name);
    }
    json!({ "type": "message", "timestamp": now_ms(), "message": inner })
}

fn write_line(file: &mut File, value: &Value) -> Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    // ponytail: a partial write (e.g. disk full) can leave a truncated line;
    // the next-turn retry then appends after the fragment, so that one line may
    // be malformed. Acceptable for best-effort persistence. Upgrade path =
    // track the byte offset and `set_len`-truncate the fragment before retry.
    file.write_all(line.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Resolve the session root: `IRIS_SESSION_DIR` override, else
/// `~/.iris/sessions`. Errors only when neither the override nor `HOME` is set.
fn sessions_root() -> Result<PathBuf> {
    if let Ok(dir) = env::var("IRIS_SESSION_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    let home = env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .context("cannot resolve session directory: HOME is not set")?;
    Ok(Path::new(&home).join(".iris/sessions"))
}

/// Flatten a working directory into a single path-safe segment, mirroring pi's
/// `/`-to-`-` slugging. Any non-alphanumeric character becomes `-`.
fn cwd_slug(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy();
    let slug: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "root".to_string()
    } else {
        trimmed.to_string()
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::{Message, ToolCall};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir {
        path: PathBuf,
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn temp_dir() -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("iris-session-test-{}-{seq}", now_ms()));
        fs::create_dir(&path).unwrap();
        TempDir { path }
    }

    fn lines(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn create_writes_a_session_header() {
        let dir = temp_dir();
        let log = SessionLog::create_in(&dir.path, Path::new("/home/dev/proj")).unwrap();
        let entries = lines(log.path());
        assert_eq!(entries[0]["type"], "session");
        assert_eq!(entries[0]["version"], SESSION_VERSION);
        assert_eq!(entries[0]["cwd"], "/home/dev/proj");
    }

    #[test]
    fn append_writes_one_message_line_per_entry() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        log.append(&Message::user("hello")).unwrap();
        log.append(&Message::assistant("hi there")).unwrap();

        let entries = lines(log.path());
        assert_eq!(entries.len(), 3); // header + 2 messages
        assert_eq!(entries[1]["message"]["role"], "user");
        assert_eq!(entries[1]["message"]["content"], "hello");
        assert_eq!(entries[2]["message"]["role"], "assistant");
        assert_eq!(entries[2]["message"]["content"], "hi there");
    }

    #[test]
    fn tool_call_entry_carries_call_id_and_name() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "read".to_string(),
            arguments: json!({ "path": "a.txt" }),
        };
        log.append(&Message::assistant_tool_call(&call)).unwrap();

        let entry = &lines(log.path())[1]["message"];
        assert_eq!(entry["role"], "assistant_tool_call");
        assert_eq!(entry["toolCallId"], "call_1");
        assert_eq!(entry["toolName"], "read");
    }

    #[test]
    fn cwd_slug_flattens_separators_and_handles_root() {
        assert_eq!(cwd_slug(Path::new("/home/dev/my-proj")), "home-dev-my-proj");
        assert_eq!(cwd_slug(Path::new("/")), "root");
    }
}
