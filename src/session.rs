//! Session transcript persistence: a tiny JSONL-backed session store.
//!
//! Each session is one JSONL file: a `session` header line followed by one
//! entry line per appended message. The shape mirrors pi-mono's session store
//! (`packages/agent/src/harness/session/`) at the smallest useful level:
//!
//! - stable **session ids** (header `id`) and **entry ids** (`id` per line),
//! - a **`parentId`** link on every entry (the previous leaf, `null` for the
//!   first), so future branching can attach to any entry,
//! - read/open a session back by id, and list sessions with metadata.
//!
//! Two halves:
//!
//! - [`SessionLog`] is the live append handle for the current run. It writes
//!   the header on create and one entry per [`append`](SessionLog::append),
//!   flushing each line so a crash leaves a valid prefix.
//! - [`SessionStore`] is the read side: [`list`](SessionStore::list) returns
//!   metadata for every persisted session, [`open`](SessionStore::open) reads
//!   one back by id with its entries in order.
//!
//! Deliberately out of scope for this slice (later milestones): tree navigation
//! and branch/leaf markers, compaction entries, labels, fork, token accounting,
//! and the `/resume` UI. The id + `parentId` shape is chosen so those can be
//! added without rewriting the on-disk format.
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
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::nexus::{Message, Role};

/// Current transcript format version. v2 adds per-entry `id` + `parentId` to the
/// v1 linear layout, so entries are tree-ready. The reader accepts older files
/// too (a missing id/parentId reads as `None`).
const SESSION_VERSION: u32 = 2;

/// An open JSONL transcript file for the current run. Each
/// [`append`](Self::append) writes one entry line and flushes, so a crash
/// leaves a valid prefix of the conversation on disk.
#[derive(Debug)]
pub(crate) struct SessionLog {
    file: File,
    path: PathBuf,
    /// Header session id (also encoded in the file name).
    id: String,
    /// Id of the last appended entry, i.e. the current leaf. The next entry's
    /// `parentId` links to it; `None` before the first append (root).
    last_id: Option<String>,
    /// Monotonic counter backing the next entry id, so ids are unique within
    /// this session by construction (no random draws, no collision check).
    next_seq: u32,
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
        Ok(Self {
            file,
            path,
            id,
            last_id: None,
            next_seq: 0,
        })
    }

    /// Append one conversation message as a `message` entry line, generating a
    /// stable entry id and linking `parentId` to the previous leaf.
    pub(crate) fn append(&mut self, message: &Message) -> Result<()> {
        let id = self.next_id();
        let entry = message_entry(&id, self.last_id.as_deref(), message);
        write_line(&mut self.file, &entry)
            .with_context(|| format!("failed to append to session {}", self.path.display()))?;
        self.last_id = Some(id);
        Ok(())
    }

    /// Session id (header `id`), used to open this session back later.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// Path of the transcript file on disk.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Generate the next entry id from the per-session counter.
    // ponytail: per-session monotonic counter, unique within one transcript
    // only (all that `parentId` linking needs). Upgrade to uuidv7 (pi's choice)
    // if entry ids ever need to be globally unique across sessions or survive a
    // fork that copies entries.
    fn next_id(&mut self) -> String {
        let id = format!("{:08x}", self.next_seq);
        self.next_seq += 1;
        id
    }
}

/// Read side of the session store, rooted at a sessions directory. Lists the
/// persisted sessions and opens one back by id. Mirrors pi-mono's
/// `JsonlSessionRepo` (`list`/`open`) at the minimal level; create lives on
/// [`SessionLog`], the live write handle.
pub(crate) struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    /// Open the store at the default resolved root (`IRIS_SESSION_DIR` or
    /// `~/.iris/sessions`).
    pub(crate) fn open_default() -> Result<Self> {
        Ok(Self::with_root(sessions_root()?))
    }

    /// Open the store at an explicit root, so tests can read without env or
    /// home-directory state.
    pub(crate) fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    /// List every persisted session's metadata, newest first. Reads only each
    /// file's header line plus its mtime, so listing stays cheap. Files that are
    /// not valid session headers are skipped, not fatal.
    pub(crate) fn list(&self) -> Result<Vec<SessionMeta>> {
        let mut sessions = Vec::new();
        let Ok(cwd_dirs) = fs::read_dir(&self.root) else {
            // Missing root just means no sessions yet.
            return Ok(sessions);
        };
        for cwd_dir in cwd_dirs.flatten() {
            if !cwd_dir.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(files) = fs::read_dir(cwd_dir.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                match read_meta(&path) {
                    Ok(meta) => sessions.push(meta),
                    Err(error) => {
                        tracing::warn!(path = %path.display(), error = %format!("{error:#}"), "skipping invalid session file");
                    }
                }
            }
        }
        sessions.sort_by_key(|meta| std::cmp::Reverse(meta.created_ms));
        Ok(sessions)
    }

    /// Open a persisted session from its listing metadata, returning the
    /// metadata plus the conversation messages in order. Mirrors pi-mono's
    /// `repo.open(metadata)`: a caller opens by id by locating the entry in
    /// [`list`](Self::list) (which carries the id) and passing it here, so
    /// opening costs one read, not a second directory scan.
    pub(crate) fn open(&self, meta: &SessionMeta) -> Result<StoredSession> {
        let messages = read_messages(&meta.path)?;
        Ok(StoredSession {
            meta: meta.clone(),
            messages,
        })
    }
}

/// Cheap listing metadata for one persisted session: enough to drive a future
/// `/resume` picker without reading the whole file.
#[derive(Debug, Clone)]
pub(crate) struct SessionMeta {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) cwd: String,
    /// Header timestamp (unix ms) the session was created at.
    pub(crate) created_ms: u128,
    /// File mtime (unix ms): a cheap "last updated" signal.
    pub(crate) updated_ms: u128,
}

/// A session read back from disk: its listing metadata plus the conversation
/// messages in order.
///
/// This reads back the linear message stream, which is what resume needs first.
/// The on-disk entries keep their `id` + `parentId` (verified at the file
/// level), so when branching/compaction land they can surface entry ids on read
/// without changing the format -- no rewrite, just a richer read result.
#[derive(Debug, Clone)]
pub(crate) struct StoredSession {
    pub(crate) meta: SessionMeta,
    pub(crate) messages: Vec<Message>,
}

/// Serialize one message into its session entry value, with a stable id and a
/// `parentId` link to the previous leaf (`null` for the first entry).
fn message_entry(id: &str, parent_id: Option<&str>, message: &Message) -> Value {
    let mut inner = json!({
        "role": message.role.as_str(),
        "content": message.content,
    });
    if let Some(call_id) = &message.tool_call_id {
        inner["toolCallId"] = json!(call_id);
    }
    if let Some(name) = &message.tool_name {
        inner["toolName"] = json!(name);
    }
    json!({
        "type": "message",
        "id": id,
        "parentId": parent_id,
        "timestamp": now_ms(),
        "message": inner,
    })
}

/// Read just the header line (and the file mtime) into listing metadata.
fn read_meta(path: &Path) -> Result<SessionMeta> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut first = String::new();
    BufReader::new(file)
        .read_line(&mut first)
        .with_context(|| format!("failed to read header of {}", path.display()))?;
    let header: Value = serde_json::from_str(first.trim())
        .with_context(|| format!("session header is not valid JSON in {}", path.display()))?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        bail!("first line is not a session header in {}", path.display());
    }
    let id = header
        .get("id")
        .and_then(Value::as_str)
        .context("session header is missing id")?
        .to_string();
    Ok(SessionMeta {
        id,
        cwd: header
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        created_ms: header.get("timestamp").and_then(as_ms).unwrap_or(0),
        updated_ms: mtime_ms(path),
        path: path.to_path_buf(),
    })
}

/// Read a full session's conversation messages in order. The header is parsed
/// and validated, then each `message` entry is reconstructed; non-message and
/// malformed lines (e.g. a truncated trailing fragment from a partial write)
/// are skipped rather than failing the whole open.
fn read_messages(path: &Path) -> Result<Vec<Message>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut lines = content.lines().filter(|line| !line.trim().is_empty());
    let header_line = lines
        .next()
        .with_context(|| format!("empty session file {}", path.display()))?;
    let header: Value = serde_json::from_str(header_line)
        .with_context(|| format!("session header is not valid JSON in {}", path.display()))?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        bail!("first line is not a session header in {}", path.display());
    }

    let mut messages = Vec::new();
    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            tracing::warn!(path = %path.display(), "skipping malformed session line");
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        if let Some(message) = value.get("message").and_then(parse_message) {
            messages.push(message);
        }
    }
    Ok(messages)
}

/// Reconstruct a [`Message`] from a persisted entry's inner `message` object.
/// `None` when the role is missing/unknown.
fn parse_message(inner: &Value) -> Option<Message> {
    let role = Role::from_wire(inner.get("role").and_then(Value::as_str)?)?;
    Some(Message {
        role,
        content: inner
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        tool_call_id: inner
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(String::from),
        tool_name: inner
            .get("toolName")
            .and_then(Value::as_str)
            .map(String::from),
    })
}

/// Parse a JSON timestamp number into unix ms. Timestamps are written as
/// [`now_ms`] (fits in `u64` for ~580M years), so `as_u64` is sufficient.
fn as_ms(value: &Value) -> Option<u128> {
    value.as_u64().map(u128::from)
}

/// File mtime as unix ms, or 0 when unavailable.
fn mtime_ms(path: &Path) -> u128 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn write_line(file: &mut File, value: &Value) -> Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    // ponytail: a partial write (e.g. disk full) can leave a truncated line;
    // the next-turn retry then appends after the fragment, so that one line may
    // be malformed. The reader skips such a fragment, so persistence stays
    // best-effort. Upgrade path = track the byte offset and `set_len`-truncate
    // the fragment before retry.
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
        assert_eq!(entries[0]["id"], log.id());
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
    fn append_assigns_ids_and_links_parent_to_previous_leaf() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        log.append(&Message::user("first")).unwrap();
        log.append(&Message::assistant("second")).unwrap();

        let entries = lines(log.path());
        let first_id = entries[1]["id"].as_str().unwrap();
        let second_id = entries[2]["id"].as_str().unwrap();
        // First entry roots the chain; second links to the first.
        assert!(entries[1]["parentId"].is_null());
        assert_eq!(entries[2]["parentId"], first_id);
        assert_ne!(first_id, second_id, "entry ids must be distinct");
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

    /// Locate a session by id in the listing and open it -- the by-id read flow
    /// (mirrors pi-mono's list + `open(metadata)`).
    fn open_by_id(store: &SessionStore, id: &str) -> StoredSession {
        let meta = store
            .list()
            .unwrap()
            .into_iter()
            .find(|meta| meta.id == id)
            .expect("session id present in listing");
        store.open(&meta).unwrap()
    }

    #[test]
    fn store_opens_a_session_by_id_and_reads_messages_in_order() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/home/dev/proj")).unwrap();
        let id = log.id().to_string();
        log.append(&Message::user("hello")).unwrap();
        log.append(&Message::assistant("hi there")).unwrap();
        drop(log);

        let store = SessionStore::with_root(dir.path.clone());
        let session = open_by_id(&store, &id);
        assert_eq!(session.meta.id, id);
        assert_eq!(session.meta.cwd, "/home/dev/proj");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].content, "hello");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "hi there");
    }

    #[test]
    fn reads_back_a_v1_file_without_entry_ids() {
        // A pre-foundation v1 transcript: entry lines carry no id/parentId.
        let dir = temp_dir();
        // list() scans cwd-slug subdirs, so place the file in one.
        let cwd_dir = dir.path.join("w");
        fs::create_dir(&cwd_dir).unwrap();
        let path = cwd_dir.join("v1.jsonl");
        let v1 = concat!(
            r#"{"type":"session","version":1,"id":"abcd1234","timestamp":1700000000000,"cwd":"/w"}"#,
            "\n",
            r#"{"type":"message","timestamp":1700000000001,"message":{"role":"user","content":"hi"}}"#,
            "\n",
            r#"{"type":"message","timestamp":1700000000002,"message":{"role":"assistant","content":"yo"}}"#,
            "\n",
        );
        fs::write(&path, v1).unwrap();

        let store = SessionStore::with_root(dir.path.clone());
        let meta = store
            .list()
            .unwrap()
            .into_iter()
            .find(|meta| meta.id == "abcd1234")
            .unwrap();
        let session = store.open(&meta).unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "hi");
        assert_eq!(session.messages[1].role, Role::Assistant);
    }

    #[test]
    fn store_lists_sessions_with_metadata_newest_first() {
        let dir = temp_dir();
        let first = SessionLog::create_in(&dir.path, Path::new("/proj/a")).unwrap();
        let first_id = first.id().to_string();
        drop(first);
        // now_ms has ms resolution; ensure a strictly later created timestamp.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = SessionLog::create_in(&dir.path, Path::new("/proj/b")).unwrap();
        let second_id = second.id().to_string();
        drop(second);

        let metas = SessionStore::with_root(dir.path.clone()).list().unwrap();
        assert_eq!(metas.len(), 2);
        // Newest first.
        assert_eq!(metas[0].id, second_id);
        assert_eq!(metas[1].id, first_id);
        assert!(metas[0].created_ms >= metas[1].created_ms);
        assert!(metas[0].path.exists());
        assert_eq!(metas[1].cwd, "/proj/a");
    }

    #[test]
    fn unknown_id_is_absent_from_the_listing() {
        let dir = temp_dir();
        let log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        drop(log);
        let store = SessionStore::with_root(dir.path.clone());
        assert!(
            store
                .list()
                .unwrap()
                .iter()
                .all(|meta| meta.id != "deadbeef")
        );
    }

    #[test]
    fn open_errors_when_the_file_is_missing() {
        let dir = temp_dir();
        let store = SessionStore::with_root(dir.path.clone());
        let meta = SessionMeta {
            id: "x".to_string(),
            path: dir.path.join("missing.jsonl"),
            cwd: "/w".to_string(),
            created_ms: 0,
            updated_ms: 0,
        };
        assert!(store.open(&meta).is_err());
    }

    #[test]
    fn list_skips_non_session_files() {
        let dir = temp_dir();
        let log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let session_dir = log.path().parent().unwrap();
        // A stray non-session jsonl file must not break listing.
        fs::write(session_dir.join("garbage.jsonl"), "not json\n").unwrap();
        let metas = SessionStore::with_root(dir.path.clone()).list().unwrap();
        assert_eq!(metas.len(), 1);
    }

    #[test]
    fn read_skips_a_malformed_trailing_line() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        log.append(&Message::user("ok")).unwrap();
        let path = log.path().to_path_buf();
        drop(log);
        // Simulate a truncated trailing fragment from a partial write.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"type\":\"message\",\"id\":\"frag\"")
            .unwrap();
        drop(file);

        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "ok");
    }

    #[test]
    fn list_returns_empty_when_root_is_missing() {
        let dir = temp_dir();
        let missing = dir.path.join("does-not-exist");
        let metas = SessionStore::with_root(missing).list().unwrap();
        assert!(metas.is_empty());
    }

    #[test]
    fn cwd_slug_flattens_separators_and_handles_root() {
        assert_eq!(cwd_slug(Path::new("/home/dev/my-proj")), "home-dev-my-proj");
        assert_eq!(cwd_slug(Path::new("/")), "root");
    }
}
