//! Session transcript persistence: a tiny JSONL-backed session store.
//!
//! Each session is one JSONL file: a `session` header line followed by one
//! entry line per appended message. The shape mirrors pi-mono's session store
//! (`packages/agent/src/harness/session/`) at the smallest useful level:
//!
//! - stable **session ids** (header `id`) and **message entry ids** (`id` per
//!   line),
//! - a **`parentId`** link on every entry (the previous leaf, `null` for the
//!   first), so future branching can attach to any entry,
//! - optional Nexus-owned **provider turn ids** (`providerTurnId`) on messages
//!   produced by one provider/model round trip,
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
//! and branch/leaf markers, labels, fork, and full token accounting. The id +
//! `parentId` shape is chosen so those can be added without rewriting the
//! on-disk format.
//!
//! A `compaction` entry records that an inclusive range of prior `message`
//! entries was summarized. [`read_messages`] replaces that covered range with
//! the summary during context rebuild, so a resumed session sees the summary
//! instead of replaying the covered turns. The entry stores enough metadata
//! (covered range, summary, `createdAt`, token-estimate placeholder) for later
//! auto-compaction/token-budget policy to attach without changing the kind.
//!
//! Location (mirrors pi's `~/.pi/agent/sessions/...`):
//!
//! ```text
//! <root>/<cwd-slug>/<unix-ms>_<id>.jsonl
//! ```
//!
//! where `<root>` is `IRIS_SESSION_DIR` if set, else `~/.iris/sessions`, and
//! `<cwd-slug>` is the working directory with path separators flattened.

use std::collections::HashMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::nexus::{Message, ModelOrigin, Role};

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
    /// Count of `compaction` entries in this transcript so far, seeded from the
    /// file on resume. The next compaction's generation ordinal (ADR-0047) is
    /// this count plus one; derived from entry order, so a session without the
    /// persisted field still counts correctly.
    compactions: u32,
}

impl SessionLog {
    /// Create a transcript with a caller-supplied session id. Used when the
    /// provider prompt-cache key must be derived before the transcript file is
    /// opened; the same opaque id then anchors both concerns.
    pub(crate) fn create_with_id(cwd: &Path, id: &str) -> Result<Self> {
        Self::create_in_with_id(&sessions_root()?, cwd, id)
    }

    /// Core constructor with an explicit session root, so tests can persist
    /// without env or home-directory state.
    #[cfg(test)]
    pub(crate) fn create_in(root: &Path, cwd: &Path) -> Result<Self> {
        let id = new_session_id();
        Self::create_in_with_id(root, cwd, &id)
    }

    /// Core constructor with an explicit session root and id, so callers and
    /// tests can bind transcript id to provider cache key deterministically.
    pub(crate) fn create_in_with_id(root: &Path, cwd: &Path, id: &str) -> Result<Self> {
        if !is_valid_session_id(id) {
            bail!("invalid session id");
        }
        let dir = root.join(cwd_slug(cwd));
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create session dir {}", dir.display()))?;
        let path = dir.join(format!("{}_{}.jsonl", now_ms(), id));
        // Create exclusively (`create_new`) so a same-millisecond id collision
        // surfaces as an error instead of silently truncating an existing
        // transcript. A collision needs the same 128-bit id drawn in the same
        // millisecond; persistence is best-effort, so erroring out on that
        // negligible chance is fine -- no retry loop needed.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
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
            id: id.to_string(),
            last_id: None,
            next_seq: 0,
            compactions: 0,
        })
    }

    /// Append one conversation message as a `message` entry line, generating a
    /// stable entry id and linking `parentId` to the previous leaf. Returns the
    /// assigned entry id so a caller can later reference it (e.g. as a
    /// compaction coverage bound) without assuming the id format.
    pub(crate) fn append(&mut self, message: &Message) -> Result<String> {
        let id = self.next_id();
        let entry = message_entry(&id, self.last_id.as_deref(), message);
        write_line(&mut self.file, &entry)
            .with_context(|| format!("failed to append to session {}", self.path.display()))?;
        self.last_id = Some(id.clone());
        Ok(id)
    }

    /// Append a `compaction` entry recording that the inclusive range of
    /// `message` entries `[covered_from, covered_to]` was summarized into
    /// `summary`. During context rebuild ([`read_messages`]) those covered
    /// messages are replaced by a single summary message, so a resumed session
    /// rebuilds context through the summary instead of replaying the range.
    ///
    /// The summary text is produced by the caller, so the entry is independent
    /// of *how* it was summarized (a deterministic internal summarizer today; a
    /// provider/local/remote summarizer later). `token_estimate` records the
    /// rebuilt body's token estimate (summary plus carry) so the rebuild counts
    /// it instead of the covered turns. The Tier-2 harness's auto-compaction
    /// policy is the production trigger (issue #55).
    ///
    /// `carry_paths` is the deterministic touched/read path carry (ADR-0044),
    /// derived from the covered range's structured tool calls, persisted as an
    /// additive optional `carryPaths` field. It is written only when non-empty,
    /// so an empty carry is byte-identical to a pre-carry compaction entry and
    /// older readers ignore it either way.
    pub(crate) fn append_compaction(
        &mut self,
        covered_from: &str,
        covered_to: &str,
        summary: &str,
        carry_paths: &[String],
        token_estimate: Option<u64>,
    ) -> Result<String> {
        let id = self.next_id();
        // Generation ordinal (ADR-0047): 1-based count of compactions in this
        // session. Compute the next generation but do not advance the counter
        // until the entry is durably written, so a failed append never leaves
        // the in-memory counter ahead of what is persisted (which would make a
        // later successful append report the wrong generation).
        let generation = self.compactions + 1;
        let mut entry = json!({
            "type": "compaction",
            "id": id,
            "parentId": self.last_id.as_deref(),
            "timestamp": now_ms(),
            "createdAt": now_ms(),
            "coveredFrom": covered_from,
            "coveredTo": covered_to,
            "summary": summary,
            // ponytail: no token-accounting convention yet, so this is an
            // explicit null placeholder. Upgrade path = write the real estimate
            // here when auto-compaction/token budgeting lands; kind unchanged.
            "tokenEstimate": token_estimate,
            // Additive optional field (ADR-0047): older readers ignore it and
            // it is recomputable from compaction-entry order, so its absence
            // never changes how a pre-ADR-0047 session rebuilds.
            "generation": generation,
        });
        // Additive optional carry (ADR-0044): only written when non-empty, so an
        // empty carry leaves the serialized entry byte-identical to a pre-carry
        // compaction and older readers are unaffected.
        if !carry_paths.is_empty() {
            entry["carryPaths"] = json!(carry_paths);
        }
        write_line(&mut self.file, &entry).with_context(|| {
            format!(
                "failed to append compaction to session {}",
                self.path.display()
            )
        })?;
        // Only advance the counter after the write succeeds; on failure the
        // `?` above returns early and `self.compactions` is left unchanged.
        self.compactions = generation;
        self.last_id = Some(id.clone());
        Ok(id)
    }

    /// Append a `modelSelection` entry recording a runtime provider/model/
    /// reasoning switch, chained onto the leaf like any other entry. This is a
    /// first-class audit record of mode switches; `read_messages` skips it (it is
    /// not a `message`/`compaction`), so it never enters the provider-visible
    /// context. `base_url` is intentionally not recorded (it is not part of the
    /// reproducible selection and may carry an override the audit log should not
    /// persist). `reasoning` is `None` when no preference is set.
    pub(crate) fn append_selection(
        &mut self,
        provider: &str,
        model: &str,
        reasoning: Option<&str>,
    ) -> Result<String> {
        let id = self.next_id();
        let entry = json!({
            "type": "modelSelection",
            "id": id,
            "parentId": self.last_id.as_deref(),
            "timestamp": now_ms(),
            "provider": provider,
            "model": model,
            "reasoning": reasoning,
        });
        write_line(&mut self.file, &entry).with_context(|| {
            format!(
                "failed to append model selection to session {}",
                self.path.display()
            )
        })?;
        self.last_id = Some(id.clone());
        Ok(id)
    }

    /// Append a `taskLifecycle` entry recording that a git-safety task opened
    /// (`event: "opened"`, ADR-0031). This is the historical audit of the
    /// task<->session join -- the only place it survives settlement (which
    /// deletes the task record). Modeled on `modelSelection`: append-only, in the
    /// leaf chain (so `scan_for_resume` chains through it and `parentId` stays
    /// intact), and skipped by `read_messages` so it never enters provider
    /// context. `body` is the opaque prompt preview captured at task open, or
    /// `None` (legacy/no description). Never an enforcement or recovery input.
    pub(crate) fn append_task_opened(
        &mut self,
        task_id: &str,
        body: Option<&str>,
    ) -> Result<String> {
        let id = self.next_id();
        let entry = json!({
            "type": "taskLifecycle",
            "id": id,
            "parentId": self.last_id.as_deref(),
            "timestamp": now_ms(),
            "event": "opened",
            "taskId": task_id,
            "body": body,
        });
        write_line(&mut self.file, &entry).with_context(|| {
            format!(
                "failed to append task-opened lifecycle to session {}",
                self.path.display()
            )
        })?;
        self.last_id = Some(id.clone());
        Ok(id)
    }

    /// Append a `taskLifecycle` entry recording that a task settled
    /// (`event: "settled"`, ADR-0031). `disposition` is a deterministic label
    /// (`accepted`/`rolledback`/`checkpointed`). Same chain/skip semantics as
    /// [`append_task_opened`](Self::append_task_opened); display-only audit,
    /// never read by enforcement or recovery.
    pub(crate) fn append_task_settled(
        &mut self,
        task_id: &str,
        disposition: &str,
    ) -> Result<String> {
        let id = self.next_id();
        let entry = json!({
            "type": "taskLifecycle",
            "id": id,
            "parentId": self.last_id.as_deref(),
            "timestamp": now_ms(),
            "event": "settled",
            "taskId": task_id,
            "disposition": disposition,
        });
        write_line(&mut self.file, &entry).with_context(|| {
            format!(
                "failed to append task-settled lifecycle to session {}",
                self.path.display()
            )
        })?;
        self.last_id = Some(id.clone());
        Ok(id)
    }

    /// Reopen an existing transcript file for append, so a resumed session
    /// continues the same log instead of starting a new one. Reads the header
    /// id and the existing entries to restore the leaf link (`parentId` of the
    /// next entry) and the id counter, so appended turns stay correctly chained
    /// and uniquely identified. Mirrors pi-mono's session repo reopening an
    /// existing session file to append future turns.
    pub(crate) fn resume(path: &Path) -> Result<Self> {
        let state = scan_for_resume(path)?;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open {} for resume", path.display()))?;
        // A prior process that crashed mid-write can leave a truncated last line
        // with no trailing newline. Appending directly would fuse the next entry
        // onto that fragment into one malformed line -- losing the first resumed
        // message too. Terminate the fragment first so it stays a single skipped
        // bad line and the new entry starts clean.
        if state.needs_newline {
            file.write_all(b"\n")
                .with_context(|| format!("failed to terminate fragment in {}", path.display()))?;
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            id: state.id,
            last_id: state.last_id,
            next_seq: state.next_seq,
            compactions: state.compactions,
        })
    }

    /// Session id (header `id`), used to open this session back later.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// Path of the transcript file on disk.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Generation ordinal (ADR-0047) of the most recent compaction in this
    /// session: the 1-based count of `compaction` entries written or seen so
    /// far. `0` before the first compaction; equals the last-appended entry's
    /// persisted `generation`. Callers read it right after `append_compaction`
    /// to surface the ordinal on `CompactionApplied`.
    pub(crate) fn compaction_generation(&self) -> u64 {
        u64::from(self.compactions)
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
        sessions.sort_by_key(|meta| std::cmp::Reverse(meta.updated_ms));
        Ok(sessions)
    }

    /// Find one persisted session by its id, returning `None` when no session
    /// with that id exists. The id-keyed entry point the `resume` CLI path uses
    /// to turn a user-supplied id into openable metadata.
    pub(crate) fn find(&self, id: &str) -> Result<Option<SessionMeta>> {
        Ok(self.list()?.into_iter().find(|meta| meta.id == id))
    }

    /// Open a persisted session from its listing metadata, returning the
    /// metadata plus the conversation messages in order. Mirrors pi-mono's
    /// `repo.open(metadata)`: a caller opens by id by locating the entry in
    /// [`list`](Self::list) (which carries the id) and passing it here, so
    /// opening costs one read, not a second directory scan.
    pub(crate) fn open(&self, meta: &SessionMeta) -> Result<StoredSession> {
        let RebuiltContext {
            messages,
            entry_ids,
            context_tokens,
        } = read_messages(&meta.path)?;
        Ok(StoredSession {
            meta: meta.clone(),
            messages,
            entry_ids,
            context_tokens,
        })
    }

    /// List resumable sessions for one workspace, newest first, each carrying a
    /// short first-user-message preview for the `/resume` picker and the
    /// `resume` listing. Reuses [`list`](Self::list) (cheap header read) then
    /// scans each matching file only up to its first user message for the
    /// preview, so listing a directory's sessions stays inexpensive.
    pub(crate) fn resumable_for_cwd(&self, cwd: &str) -> Result<Vec<ResumableSession>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|meta| meta.cwd == cwd)
            .map(|meta| {
                let preview = first_user_preview(&meta.path)
                    .unwrap_or_else(|| "(no messages yet)".to_string());
                ResumableSession { meta, preview }
            })
            .collect())
    }

    /// Find every session in `cwd`'s slug directory whose log carries a
    /// `taskLifecycle` entry for `task_id`, returning each match's id/path plus
    /// the lifecycle events it recorded (ADR-0031 session lookup, v1
    /// deterministic). The scan is bounded by the cwd-slug directory -- no
    /// index, no walk of any other cwd -- so the task<->session join stays
    /// scoped to this project. Malformed/truncated lines and non-session files
    /// are skipped, not fatal (matching the reader's never-crash stance). An
    /// unknown task id, or a cwd with no session directory yet, yields an empty
    /// vec. Read-side audit only: the session log is display-only here, never an
    /// enforcement or recovery input.
    pub(crate) fn sessions_for_task(
        &self,
        cwd: &Path,
        task_id: &str,
    ) -> Result<Vec<TaskSessionMatch>> {
        let dir = self.root.join(cwd_slug(cwd));
        let mut matches = Vec::new();
        let Ok(files) = fs::read_dir(&dir) else {
            // A cwd with no session directory yet just has no matches.
            return Ok(matches);
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(m) = scan_task_lifecycle(&path, task_id) {
                matches.push(m);
            }
        }
        // Sort by path so repeated scans return a stable order regardless of the
        // filesystem's directory-iteration order. File names begin with the
        // creation unix-ms, so this is also chronological in practice.
        matches.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(matches)
    }
}

/// One session whose log carries `taskLifecycle` entries for a given task id:
/// its header id and path plus the lifecycle events it recorded, in on-disk
/// order (ADR-0031 session lookup). Display-only audit; never an enforcement or
/// recovery input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskSessionMatch {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) events: Vec<TaskLifecycleEvent>,
}

/// A single `taskLifecycle` audit event read back from a transcript: the event
/// kind (`opened`/`settled`), the opaque body captured at open (or `None`), the
/// deterministic disposition label recorded at settle (or `None`), and the
/// entry timestamp. All fields come straight from disk, so extraction is stable
/// across reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskLifecycleEvent {
    pub(crate) event: String,
    pub(crate) body: Option<String>,
    pub(crate) disposition: Option<String>,
    pub(crate) timestamp_ms: u128,
}

/// Deterministic extraction of one session (ADR-0031 `read_session`, v1): the
/// header info plus, in on-disk order, each user message's bounded preview and
/// each `taskLifecycle` event. No summarization, no model call, no clock or
/// randomness in the output -- repeated reads of a fixed transcript produce an
/// identical value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionExtract {
    pub(crate) id: String,
    pub(crate) cwd: String,
    /// Header timestamp (unix ms) the session was created at.
    pub(crate) created_ms: u128,
    /// The extracted items in on-disk order.
    pub(crate) items: Vec<ExtractItem>,
}

/// One deterministic-extraction item, in on-disk order: a user message's
/// single-line preview or a task lifecycle event. Bounded by construction --
/// user content is only ever a [`preview_line`], never a full body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExtractItem {
    UserPreview(String),
    Lifecycle(TaskLifecycleEvent),
}

/// Scan one transcript file for `taskLifecycle` entries matching `task_id`.
/// Returns `Some` with the session's id/path and the matching events (in
/// on-disk order) when at least one is found, else `None`. A file whose first
/// line is not a session header, or that carries no matching event, yields
/// `None`; malformed/truncated lines are skipped, not fatal.
fn scan_task_lifecycle(path: &Path, task_id: &str) -> Option<TaskSessionMatch> {
    let bytes = fs::read(path).ok()?;
    let mut lines = jsonl_lines(&bytes);
    let header = lines.next().and_then(|line| line.ok())?;
    let header: Value = serde_json::from_str(header).ok()?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    let id = header.get("id").and_then(Value::as_str)?.to_string();
    let mut events = Vec::new();
    for line in lines {
        let Ok(text) = line else { continue };
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if let Some((entry_task, event)) = parse_task_lifecycle(&value)
            && entry_task == task_id
        {
            events.push(event);
        }
    }
    if events.is_empty() {
        return None;
    }
    Some(TaskSessionMatch {
        id,
        path: path.to_path_buf(),
        events,
    })
}

/// Deterministic v1 read-back of a session file: header info plus user-message
/// previews and `taskLifecycle` events in on-disk order. Bounded (previews
/// only, never a full message body) and pure (no clock/randomness), so a fixed
/// transcript extracts identically every time. Malformed/truncated lines are
/// skipped rather than failing the whole read.
pub(crate) fn extract_session(path: &Path) -> Result<SessionExtract> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut lines = jsonl_lines(&bytes);
    let header_line = lines
        .next()
        .with_context(|| format!("empty session file {}", path.display()))?
        .map_err(|_| anyhow::anyhow!("session header is not valid UTF-8 in {}", path.display()))?;
    let header: Value = serde_json::from_str(header_line)
        .with_context(|| format!("session header is not valid JSON in {}", path.display()))?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        bail!("first line is not a session header in {}", path.display());
    }
    let id = header
        .get("id")
        .and_then(Value::as_str)
        .context("session header is missing id")?
        .to_string();
    let cwd = header
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let created_ms = header.get("timestamp").and_then(as_ms).unwrap_or(0);
    let mut items = Vec::new();
    for line in lines {
        let Ok(text) = line else { continue };
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("message") => {
                let message = value.get("message");
                let role = message.and_then(|m| m.get("role")).and_then(Value::as_str);
                if role == Some("user") {
                    let content = message
                        .and_then(|m| m.get("content"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    items.push(ExtractItem::UserPreview(preview_line(content)));
                }
            }
            Some("taskLifecycle") => {
                if let Some((_, event)) = parse_task_lifecycle(&value) {
                    items.push(ExtractItem::Lifecycle(event));
                }
            }
            _ => continue,
        }
    }
    Ok(SessionExtract {
        id,
        cwd,
        created_ms,
        items,
    })
}

/// Parse a `taskLifecycle` entry into its task id and a [`TaskLifecycleEvent`].
/// `None` (skipped as not-a-lifecycle or malformed) when the type/event/taskId
/// are absent, mirroring the line-level leniency the readers use for
/// truncated/garbled entries.
fn parse_task_lifecycle(value: &Value) -> Option<(String, TaskLifecycleEvent)> {
    if value.get("type").and_then(Value::as_str) != Some("taskLifecycle") {
        return None;
    }
    let task_id = value.get("taskId").and_then(Value::as_str)?.to_string();
    let event = value.get("event").and_then(Value::as_str)?.to_string();
    // Bound the body to a single-line preview at the read boundary. It is
    // normally already a `preview_line` captured at task open (#287), but a
    // malformed / legacy / hand-edited entry could carry a full prompt; the
    // extraction and `/sessions` rendering must stay bounded (ADR-0031), never
    // a full-body dump.
    let body = value.get("body").and_then(Value::as_str).map(preview_line);
    let disposition = value
        .get("disposition")
        .and_then(Value::as_str)
        .map(String::from);
    let timestamp_ms = value.get("timestamp").and_then(as_ms).unwrap_or(0);
    Some((
        task_id,
        TaskLifecycleEvent {
            event,
            body,
            disposition,
            timestamp_ms,
        },
    ))
}

/// Split JSONL file bytes into trimmed, non-empty lines, tolerating a truncated
/// trailing fragment that splits a multibyte char (yielded as an `Err` the
/// caller skips) instead of failing the whole read. Shared by the deterministic
/// session-lookup readers.
fn jsonl_lines(bytes: &[u8]) -> impl Iterator<Item = std::result::Result<&str, ()>> {
    bytes
        .split(|&b| b == b'\n')
        .map(|line| std::str::from_utf8(line).map(str::trim).map_err(|_| ()))
        .filter(|line| !matches!(line, Ok(text) if text.is_empty()))
}

/// One resumable session's listing metadata plus a short preview of its first
/// user message, for the `/resume` picker and the plain `resume` listing.
#[derive(Debug, Clone)]
pub(crate) struct ResumableSession {
    pub(crate) meta: SessionMeta,
    pub(crate) preview: String,
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
    /// Durable message ids parallel to `messages`: `Some(id)` for a coverable
    /// on-disk entry, `None` for a summary position or an id-less legacy entry.
    /// Resume carries these into the harness so the loaded prefix stays
    /// compactable instead of being discarded as id-less (#375).
    pub(crate) entry_ids: Vec<Option<String>>,
    /// Estimated token total of the rebuilt provider-visible context, summed
    /// from the persisted per-message [`estimate_tokens`] (recomputed from
    /// content for legacy entries). Deterministic from the on-disk transcript,
    /// so reopening a session reports the same total every time. The foundation
    /// the next slice (auto-compaction budgeting) reads instead of recomputing.
    pub(crate) context_tokens: u64,
}

/// Serialize one message into its session entry value, with a stable id and a
/// `parentId` link to the previous leaf (`null` for the first entry). Each
/// entry also records a `tokenEstimate` -- the per-message token accounting this
/// foundation persists so a resumed session can report a stable context total
/// without replaying the model. Messages produced by a provider/model round
/// trip may also carry Nexus's optional `providerTurnId` correlation field.
/// The persisted `message` body for one conversation message (role, content,
/// tool linkage, reasoning continuity/origin). Split from [`message_entry`] so
/// the `/debug` snapshot can dump the in-memory context in the exact JSONL
/// message shape the transcript uses, without fabricating entry ids.
pub(crate) fn message_body(message: &Message) -> Value {
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
    // AssistantToolCall rows carry an opaque provider continuity (e.g. Gemini's
    // `thoughtSignature`) that must survive resume so the tool round-trip is not
    // rejected on the next request. Persist it the same opaque way as reasoning
    // continuity; the read path already restores `continuity` for any role.
    if message.role == Role::AssistantToolCall
        && let Some(continuity) = &message.continuity
    {
        inner["continuity"] = json!(continuity);
    }
    if message.role == Role::AssistantReasoning {
        inner["redacted"] = json!(message.redacted);
        if let Some(continuity) = &message.continuity {
            inner["continuity"] = json!(continuity);
        }
        if let Some(origin) = &message.origin {
            inner["origin"] = json!({
                "provider": origin.provider,
                "api": origin.api,
                "model": origin.model,
            });
        }
    }
    inner
}

fn message_entry(id: &str, parent_id: Option<&str>, message: &Message) -> Value {
    let inner = message_body(message);
    let mut entry = json!({
        "type": "message",
        "id": id,
        "parentId": parent_id,
        "timestamp": now_ms(),
        // Durable per-entry token accounting. Today this is a content-derived
        // estimate plus opaque reasoning continuity; the read path prefers this
        // persisted value, so swapping in real provider usage later means
        // writing the real number here without touching rebuild.
        "tokenEstimate": message_token_estimate(message),
        "message": inner,
    });
    if let Some(provider_turn_id) = &message.provider_turn_id {
        entry["providerTurnId"] = json!(provider_turn_id);
    }
    entry
}

/// Conservative content-based token estimate for one message body.
//
// ponytail: ~4 chars/token is the standard rough heuristic, rounded up so even
// a short non-empty body counts as >= 1 token (never under-count for a budget).
// It ignores role/tool framing overhead and provider-specific tokenization on
// purpose -- this foundation only needs a stable, deterministic number. Upgrade
// path = record real provider usage per turn in `tokenEstimate` and prefer it
// (the read path already does).
pub(crate) fn estimate_tokens(content: &str) -> u64 {
    (content.chars().count() as u64).div_ceil(4)
}

pub(crate) fn message_token_estimate(message: &Message) -> u64 {
    let continuity = message
        .continuity
        .as_deref()
        .map(estimate_tokens)
        .unwrap_or(0);
    estimate_tokens(&message.content).saturating_add(continuity)
}

/// Header line of the compaction carry block (ADR-0044), naming the block as
/// the deterministic touched/read path set that rides alongside the prose
/// summary.
const CARRY_BLOCK_HEADER: &str = "[files touched or read in the compacted range]";

/// Render the compaction carry (ADR-0044): a deterministic, compact block
/// listing the covered range's workspace-relative touched/read paths. Empty
/// carry renders the empty string, so [`render_compaction_body`] with no paths
/// is byte-identical to the pre-carry summary-only body.
pub(crate) fn render_carry_block(carry_paths: &[String]) -> String {
    if carry_paths.is_empty() {
        return String::new();
    }
    let mut out = format!("\n\n{CARRY_BLOCK_HEADER}");
    for path in carry_paths {
        out.push_str("\n- ");
        out.push_str(path);
    }
    out
}

/// The compaction message body rebuilt into context (ADR-0044): the prose
/// summary followed by the carry block. With an empty carry this returns the
/// summary unchanged, so an empty-carry compaction rebuilds byte-identically to
/// the pre-carry behavior. Both the live in-process rebuild
/// (`wayland::Harness::compact_range`) and the read-time rebuild
/// ([`rebuild_with_compactions`]) render through this one function so live and
/// resumed context agree.
pub(crate) fn render_compaction_body(summary: &str, carry_paths: &[String]) -> String {
    let mut out = summary.to_string();
    out.push_str(&render_carry_block(carry_paths));
    out
}

/// What [`SessionLog::resume`] needs to continue an existing transcript.
struct ResumeState {
    /// Header session id.
    id: String,
    /// Id of the last message entry (the current leaf the next `parentId` links
    /// to); `None` when there are no entries or they predate entry ids (v1).
    last_id: Option<String>,
    /// The next entry id counter. Derived from the highest existing entry id so
    /// ids stay unique even if a line was skipped, falling back to the entry
    /// count for id-less v1 files.
    next_seq: u32,
    /// Whether the file lacks a trailing newline (a truncated final fragment),
    /// so resume must terminate it before appending.
    needs_newline: bool,
    /// Count of `compaction` entries seen, so the resumed log continues the
    /// generation ordinal (ADR-0047) from entry order -- correct even for
    /// sessions written before the persisted `generation` field existed.
    compactions: u32,
}

/// Scan an existing transcript so [`SessionLog::resume`] can continue it.
fn scan_for_resume(path: &Path) -> Result<ResumeState> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let needs_newline = bytes.last().is_some_and(|&b| b != b'\n');
    let mut lines = bytes
        .split(|&b| b == b'\n')
        .map(|line| std::str::from_utf8(line).map(str::trim))
        .filter(|line| !matches!(line, Ok(text) if text.is_empty()));
    let header_line = lines
        .next()
        .with_context(|| format!("empty session file {}", path.display()))?
        .map_err(|_| anyhow::anyhow!("session header is not valid UTF-8 in {}", path.display()))?;
    let header: Value = serde_json::from_str(header_line)
        .with_context(|| format!("session header is not valid JSON in {}", path.display()))?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        bail!("first line is not a session header in {}", path.display());
    }
    let id = header
        .get("id")
        .and_then(Value::as_str)
        .context("session header is missing id")?
        .to_string();
    let mut last_id = None;
    let mut count: u32 = 0;
    let mut compactions: u32 = 0;
    let mut max_seq: Option<u32> = None;
    for line in lines {
        let Ok(text) = line else { continue };
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        // `message`, `compaction`, `modelSelection`, and `taskLifecycle` entries
        // all occupy the leaf chain and an entry-id slot, so a resumed append
        // must link its `parentId` past, and count its `next_seq` beyond,
        // whichever kind is the current leaf. (`modelSelection` and
        // `taskLifecycle` are audit records; the read/rebuild path skips them,
        // but the chain must still flow through them or `parentId` breaks.)
        match value.get("type").and_then(Value::as_str) {
            Some("message")
            | Some("compaction")
            | Some("modelSelection")
            | Some("taskLifecycle") => {}
            _ => continue,
        }
        if value.get("type").and_then(Value::as_str) == Some("compaction") {
            compactions += 1;
        }
        count += 1;
        if let Some(entry_id) = value.get("id").and_then(Value::as_str) {
            last_id = Some(entry_id.to_string());
            // Entry ids are hex of the seq counter; track the max so the next id
            // never collides even if an intermediate line was unreadable.
            if let Ok(seq) = u32::from_str_radix(entry_id, 16) {
                max_seq = Some(max_seq.map_or(seq, |m| m.max(seq)));
            }
        }
    }
    // Prefer the highest id seen (+1); fall back to the count for id-less v1
    // files. The `.max(count)` keeps the counter ahead of the entry count too.
    let next_seq = max_seq.map_or(count, |m| (m + 1).max(count));
    Ok(ResumeState {
        id,
        last_id,
        next_seq,
        needs_newline,
        compactions,
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
/// are skipped rather than failing the whole open. Also returns the rebuilt
/// context's estimated token total ([`RebuiltContext`]).
fn read_messages(path: &Path) -> Result<RebuiltContext> {
    // Read raw bytes and split on '\n' so a truncated trailing fragment that
    // splits a multibyte UTF-8 char is discarded as one bad line, rather than
    // failing the whole read -- which `read_to_string` would, since invalid
    // UTF-8 anywhere in the file errors before any line is parsed.
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut lines = bytes
        .split(|&b| b == b'\n')
        .map(|line| std::str::from_utf8(line).map(str::trim))
        .filter(|line| !matches!(line, Ok(text) if text.is_empty()));
    let header_line = lines
        .next()
        .with_context(|| format!("empty session file {}", path.display()))?
        .map_err(|_| anyhow::anyhow!("session header is not valid UTF-8 in {}", path.display()))?;
    let header: Value = serde_json::from_str(header_line)
        .with_context(|| format!("session header is not valid JSON in {}", path.display()))?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        bail!("first line is not a session header in {}", path.display());
    }

    // Collect message entries (with their ids, for compaction coverage lookup)
    // and compaction entries separately, then rebuild: covered ranges are
    // replaced by their summary so the resumed context carries the summary
    // instead of the original turns.
    let mut entries: Vec<MessageEntry> = Vec::new();
    let mut compactions: Vec<Compaction> = Vec::new();
    for line in lines {
        let Ok(text) = line else {
            tracing::warn!(path = %path.display(), "skipping non-UTF-8 session line");
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            tracing::warn!(path = %path.display(), "skipping malformed session line");
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("message") => {
                let id = value.get("id").and_then(Value::as_str).map(String::from);
                if let Some(mut message) = value.get("message").and_then(parse_message) {
                    message.provider_turn_id = value
                        .get("providerTurnId")
                        .and_then(Value::as_str)
                        .map(String::from);
                    // Prefer the persisted estimate; recompute from content for
                    // older (v1/v2-pre-token) entries that lack it, so the total
                    // is stable for both new and legacy sessions.
                    let tokens = value
                        .get("tokenEstimate")
                        .and_then(Value::as_u64)
                        .unwrap_or_else(|| message_token_estimate(&message));
                    entries.push(MessageEntry {
                        id,
                        message,
                        tokens,
                    });
                }
            }
            Some("compaction") => match parse_compaction(&value) {
                Some(compaction) => compactions.push(compaction),
                None => {
                    tracing::warn!(path = %path.display(), "skipping malformed compaction entry");
                }
            },
            _ => continue,
        }
    }
    rebuild_with_compactions(path, entries, compactions)
}

/// A message entry rebuilt from disk: its durable id (for compaction coverage
/// lookup), the reconstructed message, and the token estimate (persisted, or
/// recomputed from content for legacy entries).
struct MessageEntry {
    id: Option<String>,
    message: Message,
    tokens: u64,
}

/// A persisted compaction entry's rebuild-relevant fields: the inclusive range
/// of covered `message` entry ids, the summary that replaces them, and its
/// persisted token estimate (the summary stands in for the covered turns in the
/// context total). Other persisted metadata (`createdAt`) is durable but not
/// needed to rebuild context, so it is not read here.
struct Compaction {
    covered_from: String,
    covered_to: String,
    summary: String,
    token_estimate: Option<u64>,
    /// Deterministic touched/read path carry (ADR-0044), rendered into the
    /// rebuilt body alongside the summary. Empty for pre-carry entries and any
    /// entry whose covered range touched no in-workspace files.
    carry_paths: Vec<String>,
}

/// Parse a `compaction` entry's rebuild fields. `None` (skipped as malformed)
/// when a required field is missing, mirroring the line-level leniency for
/// truncated/garbled entries.
fn parse_compaction(value: &Value) -> Option<Compaction> {
    Some(Compaction {
        covered_from: value
            .get("coveredFrom")
            .and_then(Value::as_str)?
            .to_string(),
        covered_to: value.get("coveredTo").and_then(Value::as_str)?.to_string(),
        summary: value.get("summary").and_then(Value::as_str)?.to_string(),
        token_estimate: value.get("tokenEstimate").and_then(Value::as_u64),
        // Additive optional carry (ADR-0044): absent on pre-carry entries and on
        // empty-carry entries, both of which read as no carry. Only string
        // members are kept, so a garbled element cannot inject a non-path value.
        carry_paths: value
            .get("carryPaths")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Rebuild the provider-visible message list, replacing each compaction's
/// covered inclusive id range with a single summary message in place of the
/// first covered message.
///
/// Coverage is keyed on durable message entry ids, not array positions, so the
/// result is stable across reads. Multiple non-overlapping compactions apply
/// independently and deterministically. A range whose endpoints reference a
/// missing id, run backwards, or overlap another compaction's range is rejected
/// as invalid session data (an explicit error, like the read path's other hard
/// failures) rather than silently dropping covered turns or their summary.
//
// ponytail: the covered range is taken as given; a caller that splits a
// tool-call/tool-result pair across the boundary can leave a dangling half.
// The manual append path chooses clean boundaries today; add pair-aware range
// validation when an automatic summarizer picks ranges without that care.
fn rebuild_with_compactions(
    path: &Path,
    entries: Vec<MessageEntry>,
    compactions: Vec<Compaction>,
) -> Result<RebuiltContext> {
    if compactions.is_empty() {
        // saturating: tokenEstimate is read from disk; a corrupted/edited file
        // must not panic (debug) or wrap (release) the read, matching the rest
        // of this module's never-crash-on-bad-data stance.
        let context_tokens = entries
            .iter()
            .fold(0u64, |acc, e| acc.saturating_add(e.tokens));
        // No compaction: every entry stays verbatim, so its durable id carries
        // through 1:1 (legacy id-less entries stay `None`). Parallel to
        // `messages` so a resumed session can compact the loaded prefix.
        let entry_ids = entries.iter().map(|e| e.id.clone()).collect();
        let messages = entries.into_iter().map(|e| e.message).collect();
        return Ok(RebuiltContext {
            messages,
            entry_ids,
            context_tokens,
        });
    }
    // Owned-key map (ids are tiny) so `entries` can be consumed below without a
    // lingering borrow. Ids are unique per session by construction, so there is
    // no real collision; `collect` would otherwise keep the last seen.
    let index_of: HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.id.clone().map(|id| (id, i)))
        .collect();

    let mut covered = vec![false; entries.len()];
    // Summary (and its token estimate) to emit at the position of each range's
    // first covered message.
    let mut summary_at: Vec<Option<(String, u64)>> = vec![None; entries.len()];
    for compaction in compactions {
        let from = lookup_covered(&index_of, &compaction.covered_from, path)?;
        let to = lookup_covered(&index_of, &compaction.covered_to, path)?;
        if from > to {
            bail!(
                "compaction range {}..{} runs backwards in {}",
                compaction.covered_from,
                compaction.covered_to,
                path.display()
            );
        }
        // `from <= to` is checked above, so the inclusive range is valid.
        for slot in &mut covered[from..=to] {
            if *slot {
                bail!(
                    "overlapping compaction coverage at id {} in {}",
                    compaction.covered_from,
                    path.display()
                );
            }
            *slot = true;
        }
        // The compaction message body is the prose summary plus the carry block
        // (ADR-0044); with an empty carry it is exactly the summary, so pre-carry
        // entries rebuild unchanged. Prefer the persisted estimate (which already
        // counts summary + carry); recompute from the rendered body when absent,
        // so the body contributes its own tokens instead of the covered turns.
        let body = render_compaction_body(&compaction.summary, &compaction.carry_paths);
        let summary_tokens = compaction
            .token_estimate
            .unwrap_or_else(|| estimate_tokens(&body));
        summary_at[from] = Some((body, summary_tokens));
    }

    let mut messages = Vec::new();
    // Parallel to `messages`: `None` at each summary position (a compaction
    // entry, never itself re-coverable) and the verbatim entry's durable id
    // otherwise. This mirrors the live `compact_range` layout
    // (`wayland::Harness::compact_range`), so a resumed session ends with the
    // identical `entry_ids` shape it would have had if it had compacted
    // in-process -- the resumed prefix stays coverable and summaries still stop
    // `plan_compaction` (no summary-of-summaries).
    let mut entry_ids: Vec<Option<String>> = Vec::new();
    let mut context_tokens = 0u64;
    for (i, entry) in entries.into_iter().enumerate() {
        if let Some((summary, summary_tokens)) = summary_at[i].take() {
            // The summary stands in for the covered turns as a single user-role
            // message; providers accept it verbatim and resume continues from
            // it. The role/text choice lives only here, so swapping in a
            // provider/local summarizer later changes how the text is produced,
            // not how storage or rebuild work.
            messages.push(Message::user(&summary));
            entry_ids.push(None);
            // saturating: see the empty-compactions path above.
            context_tokens = context_tokens.saturating_add(summary_tokens);
        }
        if !covered[i] {
            context_tokens = context_tokens.saturating_add(entry.tokens);
            messages.push(entry.message);
            entry_ids.push(entry.id);
        }
    }
    Ok(RebuiltContext {
        messages,
        entry_ids,
        context_tokens,
    })
}

/// The provider-visible context rebuilt from a transcript: the message list plus
/// its estimated token total. Both come from the same pass so the total always
/// matches the messages it summed.
struct RebuiltContext {
    messages: Vec<Message>,
    /// Durable message ids parallel to `messages` (`entry_ids.len() ==
    /// messages.len()`): `Some(id)` for a verbatim, coverable entry, `None` for
    /// a summary position or a genuinely id-less legacy entry. Threaded to the
    /// resume path so the loaded prefix stays compactable (#375).
    entry_ids: Vec<Option<String>>,
    context_tokens: u64,
}

/// Resolve a compaction coverage endpoint id to its message index, erroring
/// when the id is not a known message entry in this session.
fn lookup_covered(index_of: &HashMap<String, usize>, id: &str, path: &Path) -> Result<usize> {
    index_of.get(id).copied().with_context(|| {
        format!(
            "compaction covers unknown message id {} in {}",
            id,
            path.display()
        )
    })
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
        provider_turn_id: None,
        continuity: inner
            .get("continuity")
            .and_then(Value::as_str)
            .map(String::from),
        redacted: inner
            .get("redacted")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        origin: inner.get("origin").and_then(parse_origin),
    })
}

fn parse_origin(value: &Value) -> Option<ModelOrigin> {
    Some(ModelOrigin::new(
        value.get("provider").and_then(Value::as_str)?,
        value.get("api").and_then(Value::as_str)?,
        value.get("model").and_then(Value::as_str)?,
    ))
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

pub(crate) fn new_session_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// Pick the newest session for `cwd` from a [`list`](SessionStore::list) result.
/// `metas` is assumed newest-updated-first (as `list` returns), so the first
/// match for the directory is the most recently active one. `None` when the directory has no
/// persisted session. Pure so `iris --continue` selection is unit-tested without
/// disk.
pub(crate) fn newest_for_cwd<'a>(metas: &'a [SessionMeta], cwd: &str) -> Option<&'a SessionMeta> {
    metas.iter().find(|meta| meta.cwd == cwd)
}

/// Maximum characters kept in a session preview before an ellipsis.
const PREVIEW_CHARS: usize = 80;

/// Collapse a message body into a single-line preview: runs of whitespace
/// (including newlines) become one space, and the result is truncated to
/// [`PREVIEW_CHARS`] with a trailing ellipsis. Pure and char-boundary safe.
pub(crate) fn preview_line(content: &str) -> String {
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= PREVIEW_CHARS {
        collapsed
    } else {
        let kept: String = collapsed.chars().take(PREVIEW_CHARS).collect();
        format!("{kept}…")
    }
}

/// Read a session file only far enough to extract a single-line preview of its
/// first user message. Returns `None` when the file cannot be read or has no
/// user message yet. Stops at the first user entry, so it never reads a whole
/// transcript for the preview.
fn first_user_preview(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    for line in BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
    {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let message = value.get("message");
        let role = message.and_then(|m| m.get("role")).and_then(Value::as_str);
        if role == Some("user") {
            let content = message
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("");
            return Some(preview_line(content));
        }
    }
    None
}

/// A short, human-relative age (`just now`, `5m ago`, `3h ago`, `2d ago`) for a
/// session timestamp, measured against `now_ms`. Pure so the picker/list
/// formatting is unit-tested without a clock. A future or malformed timestamp
/// reads as `just now`.
pub(crate) fn relative_age(now_ms: u128, timestamp_ms: u128) -> String {
    let delta = now_ms.saturating_sub(timestamp_ms);
    let seconds = delta / 1000;
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

/// Current unix time in milliseconds, for age formatting at the call site.
pub(crate) fn current_ms() -> u128 {
    now_ms()
}

fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
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
    use crate::nexus::{Message, ModelOrigin, ToolCall};
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
    fn create_with_id_uses_supplied_id_for_header_and_filename() {
        let dir = temp_dir();
        let log = SessionLog::create_in_with_id(&dir.path, Path::new("/w"), "abc123ef").unwrap();
        let entries = lines(log.path());
        assert_eq!(log.id(), "abc123ef");
        assert_eq!(entries[0]["id"], "abc123ef");
        assert!(
            log.path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("abc123ef")
        );
    }

    #[test]
    fn generated_session_ids_are_128_bit_hex() {
        let id = new_session_id();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn create_with_id_rejects_path_like_ids() {
        let dir = temp_dir();
        let error = SessionLog::create_in_with_id(&dir.path, Path::new("/w"), "../bad")
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid session id"));
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
    fn reasoning_entry_round_trips_continuity_origin_and_tokens() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let origin = ModelOrigin::new("anthropic", "anthropic-messages", "claude-sonnet-4-6");
        log.append(&Message::assistant_reasoning(
            "",
            "opaque-redacted",
            true,
            origin.clone(),
        ))
        .unwrap();
        drop(log);

        let raw = lines(
            &SessionStore::with_root(dir.path.clone())
                .find(&id)
                .unwrap()
                .unwrap()
                .path,
        );
        let entry = &raw[1];
        assert_eq!(entry["message"]["role"], "assistant_reasoning");
        assert_eq!(entry["message"]["content"], "");
        assert_eq!(entry["message"]["continuity"], "opaque-redacted");
        assert_eq!(entry["message"]["redacted"], true);
        assert_eq!(entry["message"]["origin"]["provider"], "anthropic");
        assert_eq!(entry["message"]["origin"]["api"], "anthropic-messages");
        assert_eq!(entry["message"]["origin"]["model"], "claude-sonnet-4-6");
        assert_eq!(
            entry["tokenEstimate"],
            json!(estimate_tokens("opaque-redacted"))
        );

        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        assert_eq!(session.messages.len(), 1);
        let message = &session.messages[0];
        assert_eq!(message.role, Role::AssistantReasoning);
        assert_eq!(message.content, "");
        assert_eq!(message.continuity.as_deref(), Some("opaque-redacted"));
        assert!(message.redacted);
        assert_eq!(message.origin.as_ref(), Some(&origin));
        assert_eq!(session.context_tokens, estimate_tokens("opaque-redacted"));
    }

    #[test]
    fn tool_call_entry_carries_call_id_and_name() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let call = ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
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
    fn tool_call_entry_round_trips_thought_signature_continuity() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "ls".to_string(),
            arguments: json!({ "path": "." }),
            thought_signature: Some("sig-xyz".to_string()),
        };
        log.append(&Message::assistant_tool_call(&call)).unwrap();
        drop(log);

        let store = SessionStore::with_root(dir.path.clone());
        let entry = &lines(&store.find(&id).unwrap().unwrap().path)[1]["message"];
        assert_eq!(entry["continuity"], "sig-xyz");

        // The signature survives a full resume so the next request can echo it.
        let session = open_by_id(&store, &id);
        assert_eq!(session.messages[0].continuity.as_deref(), Some("sig-xyz"));
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
    fn resume_appends_to_the_same_file_with_linked_ids() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        log.append(&Message::user("one")).unwrap();
        log.append(&Message::assistant("two")).unwrap();
        let path = log.path().to_path_buf();
        let id = log.id().to_string();
        drop(log);

        // Reopen the same transcript and continue it.
        let mut resumed = SessionLog::resume(&path).unwrap();
        assert_eq!(resumed.path(), path);
        assert_eq!(resumed.id(), id);
        resumed.append(&Message::user("three")).unwrap();
        drop(resumed);

        let entries = lines(&path);
        assert_eq!(entries.len(), 4); // header + 3 messages, same file
        let second_id = entries[2]["id"].as_str().unwrap();
        let third_id = entries[3]["id"].as_str().unwrap();
        assert_eq!(entries[3]["message"]["content"], "three");
        // The continued entry links to the prior leaf and gets a fresh id.
        assert_eq!(entries[3]["parentId"], second_id);
        assert_ne!(third_id, second_id);
    }

    #[test]
    fn resume_after_a_truncated_fragment_keeps_the_first_new_message() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        log.append(&Message::user("kept")).unwrap();
        let path = log.path().to_path_buf();
        drop(log);
        // Simulate a crash mid-write: a truncated final line with no newline.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"type\":\"message\",\"id\"").unwrap();
        drop(file);

        // Resume and append: the fragment must not swallow the new message.
        let mut resumed = SessionLog::resume(&path).unwrap();
        resumed.append(&Message::assistant("survives")).unwrap();
        drop(resumed);

        let store = SessionStore::with_root(dir.path.clone());
        let session = open_by_id(&store, &id);
        let contents: Vec<&str> = session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(contents, ["kept", "survives"]);
    }

    #[test]
    fn find_returns_metadata_by_id_and_none_for_unknown() {
        let dir = temp_dir();
        let log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        drop(log);
        let store = SessionStore::with_root(dir.path.clone());
        assert_eq!(store.find(&id).unwrap().unwrap().id, id);
        assert!(store.find("deadbeef").unwrap().is_none());
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
    fn read_skips_a_trailing_fragment_that_splits_a_utf8_char() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        // A full non-ASCII line round-trips fine.
        log.append(&Message::user("\u{4f60}\u{597d}")).unwrap();
        let path = log.path().to_path_buf();
        drop(log);
        // A crash mid-write can leave a fragment whose bytes are an incomplete
        // multibyte char (the first two bytes of "\u{1F600}"). read_to_string
        // would reject the whole file; the byte reader must skip only this line.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[0xF0, 0x9F]).unwrap();
        drop(file);

        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "\u{4f60}\u{597d}");
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

    /// Message contents in rebuild order -- the provider-visible context shape.
    fn contents(session: &StoredSession) -> Vec<String> {
        session.messages.iter().map(|m| m.content.clone()).collect()
    }

    /// Expected context total for a message list, summed with the same
    /// per-message convention the read path persists and rebuilds with
    /// ([`estimate_tokens`]). The live-side comparison for resume stability.
    fn total(messages: &[Message]) -> u64 {
        messages.iter().map(message_token_estimate).sum()
    }

    #[test]
    fn model_selection_entry_is_written_chained_and_skipped_by_read() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let user_id = log.append(&Message::user("hello")).unwrap();
        let sel_id = log
            .append_selection("anthropic", "claude-sonnet-4-6", Some("high"))
            .unwrap();
        log.append(&Message::assistant("hi")).unwrap();
        let path = log.path().to_path_buf();
        drop(log);

        // The raw line is a first-class audit entry chained onto the leaf, and
        // base_url is intentionally absent.
        let entries = lines(&path);
        let sel = entries
            .iter()
            .find(|e| e["type"] == "modelSelection")
            .expect("modelSelection entry present");
        assert_eq!(sel["id"], sel_id);
        assert_eq!(sel["parentId"], user_id);
        assert_eq!(sel["provider"], "anthropic");
        assert_eq!(sel["model"], "claude-sonnet-4-6");
        assert_eq!(sel["reasoning"], "high");
        assert!(
            sel.get("baseUrl").is_none(),
            "base_url must not be recorded"
        );
        // The assistant entry chained through the selection entry.
        let assistant = entries.last().unwrap();
        assert_eq!(assistant["parentId"], sel_id);

        // read_messages ignores the modelSelection line: only the two real
        // messages are reconstructed, in order.
        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        assert_eq!(contents(&session), ["hello", "hi"]);
    }

    // Issue #287 test (4), session-log side: `taskLifecycle` entries are
    // first-class audit records chained onto the leaf, skipped by
    // `read_messages` (never provider-visible), and resume chains its `parentId`
    // through a `taskLifecycle` leaf so the chain stays intact.
    #[test]
    fn task_lifecycle_entries_are_chained_and_skipped_by_read() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let user_id = log.append(&Message::user("please fix it")).unwrap();
        let opened_id = log
            .append_task_opened("task-1", Some("please fix it"))
            .unwrap();
        let assistant_id = log.append(&Message::assistant("done")).unwrap();
        let settled_id = log.append_task_settled("task-1", "accepted").unwrap();
        let path = log.path().to_path_buf();
        drop(log);

        let entries = lines(&path);
        // The opened entry is a first-class audit record chained onto the user
        // message, carrying its opaque body.
        let opened = entries
            .iter()
            .find(|e| e["type"] == "taskLifecycle" && e["event"] == "opened")
            .expect("taskLifecycle opened entry present");
        assert_eq!(opened["id"], opened_id);
        assert_eq!(opened["parentId"], user_id);
        assert_eq!(opened["taskId"], "task-1");
        assert_eq!(opened["body"], "please fix it");
        assert!(
            opened.get("disposition").is_none(),
            "an opened entry carries no disposition"
        );
        // The assistant message chained THROUGH the opened lifecycle entry.
        assert_eq!(entries[3]["parentId"], opened_id);

        let settled = entries
            .iter()
            .find(|e| e["type"] == "taskLifecycle" && e["event"] == "settled")
            .expect("taskLifecycle settled entry present");
        assert_eq!(settled["id"], settled_id);
        assert_eq!(settled["parentId"], assistant_id);
        assert_eq!(settled["taskId"], "task-1");
        assert_eq!(settled["disposition"], "accepted");

        // read_messages excludes both lifecycle entries: only the two real
        // messages are reconstructed, in order.
        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        assert_eq!(contents(&session), ["please fix it", "done"]);

        // Resume after a `taskLifecycle` leaf keeps the parentId chain intact:
        // the next appended entry links to the lifecycle leaf, not past it.
        let mut resumed = SessionLog::resume(&path).unwrap();
        let next_id = resumed.append(&Message::user("thanks")).unwrap();
        assert_ne!(next_id, settled_id, "the resumed entry gets a fresh id");
        let resumed_entries = lines(&path);
        let last = resumed_entries.last().unwrap();
        assert_eq!(
            last["parentId"], settled_id,
            "resume chains parentId through the taskLifecycle leaf"
        );
    }

    /// Render extraction items to short strings so tests can assert on-disk
    /// order and content without matching enum shapes inline.
    fn describe_items(items: &[ExtractItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                ExtractItem::UserPreview(preview) => format!("user: {preview}"),
                ExtractItem::Lifecycle(ev) => match ev.event.as_str() {
                    "opened" => format!("opened: {}", ev.body.as_deref().unwrap_or("(none)")),
                    "settled" => {
                        format!("settled: {}", ev.disposition.as_deref().unwrap_or("(none)"))
                    }
                    other => format!("{other}: ?"),
                },
            })
            .collect()
    }

    // Issue #289 test (1): `sessions_for_task` finds every session whose log
    // carries the task id, scoped to the cwd slug; unknown task id => empty.
    #[test]
    fn sessions_for_task_finds_all_sessions_carrying_the_task_id() {
        let dir = temp_dir();
        let mut a = SessionLog::create_in(&dir.path, Path::new("/proj")).unwrap();
        a.append(&Message::user("start")).unwrap();
        a.append_task_opened("task-42", Some("fix login")).unwrap();
        let a_id = a.id().to_string();
        let session_dir = a.path().parent().unwrap().to_path_buf();
        drop(a);
        let mut b = SessionLog::create_in(&dir.path, Path::new("/proj")).unwrap();
        b.append_task_opened("task-42", None).unwrap();
        b.append_task_settled("task-42", "accepted").unwrap();
        let b_id = b.id().to_string();
        drop(b);
        // A session that never touched the task must not match.
        let mut c = SessionLog::create_in(&dir.path, Path::new("/proj")).unwrap();
        c.append(&Message::user("unrelated")).unwrap();
        drop(c);
        // A stray non-session file in the same dir must be skipped, not fatal.
        fs::write(session_dir.join("garbage.jsonl"), "not json\n").unwrap();

        let store = SessionStore::with_root(dir.path.clone());
        let matches = store
            .sessions_for_task(Path::new("/proj"), "task-42")
            .unwrap();
        let ids: Vec<&str> = matches.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(matches.len(), 2, "both sessions working task-42 match");
        assert!(ids.contains(&a_id.as_str()));
        assert!(ids.contains(&b_id.as_str()));
        // Unknown task id => empty.
        assert!(
            store
                .sessions_for_task(Path::new("/proj"), "nope")
                .unwrap()
                .is_empty()
        );
    }

    // Issue #289 test (1, scope): the scan is bounded to the cwd slug; the same
    // task id in another cwd is never returned, and a cwd with no session
    // directory yields empty rather than erroring.
    #[test]
    fn sessions_for_task_is_scoped_to_the_cwd_slug() {
        let dir = temp_dir();
        let mut here = SessionLog::create_in(&dir.path, Path::new("/here")).unwrap();
        here.append_task_opened("shared-id", Some("here")).unwrap();
        drop(here);
        let mut elsewhere = SessionLog::create_in(&dir.path, Path::new("/elsewhere")).unwrap();
        elsewhere
            .append_task_opened("shared-id", Some("elsewhere"))
            .unwrap();
        drop(elsewhere);

        let store = SessionStore::with_root(dir.path.clone());
        let matches = store
            .sessions_for_task(Path::new("/here"), "shared-id")
            .unwrap();
        assert_eq!(matches.len(), 1, "only the /here session, not /elsewhere");
        assert_eq!(matches[0].events.len(), 1);
        assert_eq!(matches[0].events[0].body.as_deref(), Some("here"));
        // A cwd with no session directory yet is empty, not an error.
        assert!(
            store
                .sessions_for_task(Path::new("/never"), "shared-id")
                .unwrap()
                .is_empty()
        );
    }

    // Issue #289 test (1, events): a match carries its lifecycle events in
    // on-disk order with body/disposition preserved.
    #[test]
    fn sessions_for_task_collects_lifecycle_events_in_order() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        log.append(&Message::user("please fix it")).unwrap();
        log.append_task_opened("t1", Some("please fix it")).unwrap();
        log.append(&Message::assistant("done")).unwrap();
        log.append_task_settled("t1", "accepted").unwrap();
        drop(log);

        let store = SessionStore::with_root(dir.path.clone());
        let matches = store.sessions_for_task(Path::new("/w"), "t1").unwrap();
        assert_eq!(matches.len(), 1);
        let events = &matches[0].events;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "opened");
        assert_eq!(events[0].body.as_deref(), Some("please fix it"));
        assert!(events[0].disposition.is_none());
        assert_eq!(events[1].event, "settled");
        assert_eq!(events[1].disposition.as_deref(), Some("accepted"));
        assert!(events[1].body.is_none());
    }

    // Issue #289 test (2): deterministic extraction is stable across reads and
    // emits header info + user previews + lifecycle events in on-disk order
    // (assistant turns are not previewed -- user messages only).
    #[test]
    fn extract_session_is_deterministic_across_reads() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        log.append(&Message::user("first request")).unwrap();
        log.append_task_opened("t1", Some("first request")).unwrap();
        log.append(&Message::assistant("working")).unwrap();
        log.append(&Message::user("second request")).unwrap();
        log.append_task_settled("t1", "accepted").unwrap();
        let path = log.path().to_path_buf();
        let id = log.id().to_string();
        drop(log);

        let first = extract_session(&path).unwrap();
        let second = extract_session(&path).unwrap();
        assert_eq!(first, second, "extraction must be stable across reads");
        assert_eq!(first.id, id);
        assert_eq!(first.cwd, "/w");
        assert_eq!(
            describe_items(&first.items),
            vec![
                "user: first request".to_string(),
                "opened: first request".to_string(),
                "user: second request".to_string(),
                "settled: accepted".to_string(),
            ]
        );
    }

    // Issue #289 test (3): malformed/truncated session lines are skipped, not
    // fatal (matches the existing reader stance).
    #[test]
    fn extract_session_skips_malformed_and_truncated_lines() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        log.append(&Message::user("kept")).unwrap();
        log.append_task_opened("t1", None).unwrap();
        let path = log.path().to_path_buf();
        drop(log);
        // A garbage line, then a truncated fragment with no trailing newline.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"not json at all\n").unwrap();
        file.write_all(b"{\"type\":\"message\",\"id\"").unwrap();
        drop(file);

        let extract = extract_session(&path).unwrap();
        assert_eq!(
            describe_items(&extract.items),
            vec!["user: kept".to_string(), "opened: (none)".to_string()],
            "malformed and truncated lines are skipped, not fatal"
        );
    }

    // Issue #289 test (4): extraction stays bounded -- a huge user body becomes
    // a bounded preview, never a full-body dump.
    #[test]
    fn extract_session_stays_bounded_to_previews_for_a_large_transcript() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let huge = "x".repeat(10_000);
        log.append(&Message::user(&huge)).unwrap();
        let path = log.path().to_path_buf();
        drop(log);

        let extract = extract_session(&path).unwrap();
        assert_eq!(extract.items.len(), 1);
        let ExtractItem::UserPreview(preview) = &extract.items[0] else {
            panic!("expected a user preview");
        };
        assert!(
            preview.chars().count() <= PREVIEW_CHARS + 1,
            "preview must be bounded, got {} chars",
            preview.chars().count()
        );
        assert!(
            !preview.contains(&huge),
            "the full body must never appear in the extraction"
        );
    }

    // Issue #289 (review fix): a huge/legacy `taskLifecycle` body is bounded to a
    // preview at the read boundary, so neither `sessions_for_task` nor
    // `extract_session` (and thus `/sessions`) can surface a full-body dump.
    #[test]
    fn lifecycle_body_is_bounded_at_read() {
        let dir = temp_dir();
        let huge = "x".repeat(10_000);
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        log.append_task_opened("task-huge", Some(&huge)).unwrap();
        let path = log.path().to_path_buf();
        drop(log);

        // Via sessions_for_task: the event body is a bounded preview.
        let store = SessionStore::with_root(dir.path.clone());
        let matches = store
            .sessions_for_task(Path::new("/w"), "task-huge")
            .unwrap();
        let body = matches[0].events[0].body.as_deref().unwrap();
        assert!(
            body.chars().count() <= PREVIEW_CHARS + 1,
            "lookup lifecycle body must be bounded, got {} chars",
            body.chars().count()
        );
        assert!(
            !body.contains(&huge),
            "the full lifecycle body never appears"
        );

        // Via extract_session: the lifecycle item body is likewise bounded.
        let extract = extract_session(&path).unwrap();
        let dumped_full = extract.items.iter().any(|item| match item {
            ExtractItem::Lifecycle(ev) => ev.body.as_deref() == Some(huge.as_str()),
            _ => false,
        });
        assert!(
            !dumped_full,
            "extraction never emits the full lifecycle body"
        );
    }

    #[test]
    fn append_compaction_writes_a_compaction_entry_with_durable_metadata() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let from = log.append(&Message::user("alpha")).unwrap();
        let to = log.append(&Message::assistant("beta")).unwrap();
        let compaction_id = log
            .append_compaction(&from, &to, "summary text", &[], None)
            .unwrap();

        let entries = lines(log.path());
        let entry = entries.last().unwrap();
        assert_eq!(entry["type"], "compaction");
        assert_eq!(entry["id"], compaction_id);
        // The compaction links onto the leaf it summarizes, keeping the chain.
        assert_eq!(entry["parentId"], to);
        assert_eq!(entry["coveredFrom"], from);
        assert_eq!(entry["coveredTo"], to);
        assert_eq!(entry["summary"], "summary text");
        assert!(entry["createdAt"].is_u64());
        // Token estimate is an explicit upgrade-safe placeholder until a token
        // convention exists.
        assert!(entry["tokenEstimate"].is_null());
        // The first compaction in the session persists generation 1 (ADR-0047).
        assert_eq!(entry["generation"], 1);
    }

    #[test]
    fn append_compaction_increments_the_generation_ordinal() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let a = log.append(&Message::user("a")).unwrap();
        let b = log.append(&Message::assistant("b")).unwrap();
        let c = log.append(&Message::user("c")).unwrap();
        log.append_compaction(&a, &a, "S1", &[], None).unwrap();
        assert_eq!(log.compaction_generation(), 1);
        log.append_compaction(&b, &c, "S2", &[], None).unwrap();
        assert_eq!(log.compaction_generation(), 2);

        let entries = lines(log.path());
        let generations: Vec<&Value> = entries
            .iter()
            .filter(|e| e["type"] == "compaction")
            .map(|e| &e["generation"])
            .collect();
        assert_eq!(generations, [&Value::from(1), &Value::from(2)]);
    }

    // Injecting a write failure needs a writable path whose writes always fail;
    // `/dev/full` (Linux) accepts opens but returns ENOSPC on every write, so it
    // is the cleanest failure seam without a fake writer trait.
    #[cfg(target_os = "linux")]
    #[test]
    fn failed_compaction_append_does_not_advance_the_generation() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let path = log.path().to_path_buf();
        let a = log.append(&Message::user("a")).unwrap();

        // Redirect writes to /dev/full so the next append fails mid-write while
        // the in-memory counter is still 0.
        log.file = fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        assert!(
            log.append_compaction(&a, &a, "S1", &[], None).is_err(),
            "write to /dev/full must fail"
        );
        // The failed append must not have advanced the counter.
        assert_eq!(log.compaction_generation(), 0);

        // Restore a real writable handle; the first durable compaction after a
        // failed attempt must still report generation 1, not 2.
        log.file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        log.append_compaction(&a, &a, "S1", &[], None).unwrap();
        assert_eq!(log.compaction_generation(), 1);

        let entries = lines(log.path());
        let generations: Vec<&Value> = entries
            .iter()
            .filter(|e| e["type"] == "compaction")
            .map(|e| &e["generation"])
            .collect();
        // Exactly one durable compaction entry, persisting generation 1.
        assert_eq!(generations, [&Value::from(1)]);
    }

    #[test]
    fn resume_continues_the_compaction_generation_count() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let a = log.append(&Message::user("a")).unwrap();
        log.append_compaction(&a, &a, "S1", &[], None).unwrap();
        assert_eq!(log.compaction_generation(), 1);
        let path = log.path().to_path_buf();
        drop(log);

        // Resume seeds the counter from the one prior compaction entry, so the
        // next compaction continues at generation 2 rather than restarting.
        let mut resumed = SessionLog::resume(&path).unwrap();
        let b = resumed.append(&Message::user("b")).unwrap();
        resumed.append_compaction(&b, &b, "S2", &[], None).unwrap();
        assert_eq!(resumed.compaction_generation(), 2);
        drop(resumed);

        let generations: Vec<Value> = lines(&path)
            .into_iter()
            .filter(|e| e["type"] == "compaction")
            .map(|e| e["generation"].clone())
            .collect();
        assert_eq!(generations, [Value::from(1), Value::from(2)]);
    }

    #[test]
    fn legacy_compaction_without_generation_rebuilds_unchanged() {
        // A session written before ADR-0047 has no `generation` field on its
        // compaction entry. Rebuild must be byte-for-byte unaffected by the new
        // optional field: same messages, and the read never rewrites the file.
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let from = log.append(&Message::user("alpha")).unwrap();
        let to = log.append(&Message::assistant("beta")).unwrap();
        let leaf = log.append(&Message::user("gamma")).unwrap();
        let path = log.path().to_path_buf();
        drop(log);

        // Hand-append a legacy compaction entry with no `generation` field.
        let legacy = json!({
            "type": "compaction",
            "id": "deadbeef",
            "parentId": leaf,
            "timestamp": 1,
            "createdAt": 1,
            "coveredFrom": from,
            "coveredTo": to,
            "summary": "LEGACY",
            "tokenEstimate": Value::Null,
        });
        let mut line = serde_json::to_string(&legacy).unwrap();
        line.push('\n');
        {
            use std::io::Write;
            let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(line.as_bytes()).unwrap();
        }
        let before = fs::read(&path).unwrap();

        let store = SessionStore::with_root(dir.path.clone());
        let session = open_by_id(&store, &id);
        assert_eq!(contents(&session), ["LEGACY", "gamma"]);
        // Reading the legacy session did not rewrite it.
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn rebuild_replaces_the_covered_range_with_the_summary() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let from = log.append(&Message::user("alpha")).unwrap();
        let to = log.append(&Message::assistant("beta")).unwrap();
        log.append(&Message::user("gamma")).unwrap();
        log.append(&Message::assistant("delta")).unwrap();
        log.append_compaction(&from, &to, "SUMMARY", &[], None)
            .unwrap();
        drop(log);

        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        // The covered turns are gone, replaced in place by one summary message;
        // the uncovered tail is preserved. Fails if covered turns are replayed.
        assert_eq!(contents(&session), ["SUMMARY", "gamma", "delta"]);
        assert_eq!(session.messages[0].role, Role::User);
        assert!(
            session
                .messages
                .iter()
                .all(|m| m.content != "alpha" && m.content != "beta"),
            "covered turns must not be replayed alongside the summary"
        );
    }

    #[test]
    fn append_compaction_writes_carry_paths_when_present() {
        // ADR-0044: the carry is an additive optional `carryPaths` field; the
        // prose `summary` stays carry-free, and the existing fields are unchanged.
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let from = log.append(&Message::user("alpha")).unwrap();
        let to = log.append(&Message::assistant("beta")).unwrap();
        let carry = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        log.append_compaction(&from, &to, "prose only", &carry, Some(9))
            .unwrap();

        let entry = lines(log.path()).pop().unwrap();
        assert_eq!(entry["summary"], "prose only");
        assert_eq!(entry["carryPaths"], json!(["src/a.rs", "src/b.rs"]));
        assert_eq!(entry["tokenEstimate"], json!(9));
        assert_eq!(entry["generation"], 1);
    }

    #[test]
    fn empty_carry_writes_no_field_and_stays_byte_identical() {
        // ADR-0044 item 5: an empty carry writes no `carryPaths` key, so the
        // serialized compaction entry is byte-identical to the pre-carry entry
        // and older readers are unaffected.
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let from = log.append(&Message::user("alpha")).unwrap();
        let to = log.append(&Message::assistant("beta")).unwrap();
        log.append_compaction(&from, &to, "SUMMARY", &[], None)
            .unwrap();
        drop(log);

        // The raw persisted line carries no carry field at all.
        let raw = fs::read_to_string(
            SessionStore::with_root(dir.path.clone())
                .find(&id)
                .unwrap()
                .unwrap()
                .path,
        )
        .unwrap();
        assert!(
            !raw.contains("carryPaths"),
            "empty carry must not serialize a carryPaths field"
        );
        let entry: Value = raw
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|v| v["type"] == "compaction")
            .unwrap();
        assert!(
            !entry.as_object().unwrap().contains_key("carryPaths"),
            "empty carry must not add a carryPaths key"
        );

        // Rebuild is unchanged: summary in, no carry block.
        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        assert_eq!(contents(&session), ["SUMMARY"]);
    }

    #[test]
    fn rebuild_renders_summary_and_carry_and_counts_the_carry_tokens() {
        // ADR-0044 item 3: rebuild renders summary + carry, and the carry's
        // tokens are counted in the entry estimate the rebuilt total uses.
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let from = log.append(&Message::user("alpha")).unwrap();
        let to = log.append(&Message::assistant("beta")).unwrap();
        let carry = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        // Mirror the live path: the persisted estimate is the rendered body's.
        let body = render_compaction_body("SUMMARY", &carry);
        log.append_compaction(&from, &to, "SUMMARY", &carry, Some(estimate_tokens(&body)))
            .unwrap();
        drop(log);

        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        // One rebuilt message: the summary plus the carry block verbatim.
        assert_eq!(session.messages.len(), 1);
        let rebuilt = &session.messages[0].content;
        assert!(rebuilt.starts_with("SUMMARY"));
        assert!(rebuilt.contains("src/a.rs") && rebuilt.contains("src/b.rs"));
        assert_eq!(*rebuilt, body);
        // The total counts the summary + carry body, not the covered turns.
        assert_eq!(session.context_tokens, estimate_tokens(&body));
    }

    #[test]
    fn carry_paths_survive_a_summary_that_omits_them() {
        // ADR-0044 item 4 (retention contract that retires ADR-0041's named
        // risk): a load-bearing path dropped from the prose summary still
        // survives in the carry through rebuild.
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let from = log.append(&Message::user("alpha")).unwrap();
        let to = log.append(&Message::assistant("beta")).unwrap();
        // The summary deliberately omits the load-bearing path.
        let summary = "the agent finished the task";
        let carry = vec!["src/load_bearing.rs".to_string()];
        assert!(!summary.contains("load_bearing"));
        let body = render_compaction_body(summary, &carry);
        log.append_compaction(&from, &to, summary, &carry, Some(estimate_tokens(&body)))
            .unwrap();
        drop(log);

        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        assert!(
            session.messages[0].content.contains("src/load_bearing.rs"),
            "the carried path must survive a summary that dropped it"
        );
    }

    #[test]
    fn compaction_entry_without_carry_field_rebuilds() {
        // ADR-0044 item 2 (backward compat): a pre-carry compaction entry with
        // no carryPaths field rebuilds exactly as before.
        let dir = temp_dir();
        let cwd_dir = dir.path.join("w");
        fs::create_dir(&cwd_dir).unwrap();
        let path = cwd_dir.join("legacy.jsonl");
        let legacy = concat!(
            r#"{"type":"session","version":2,"id":"nocarry1","timestamp":1700000000000,"cwd":"/w"}"#,
            "\n",
            r#"{"type":"message","id":"00000000","parentId":null,"timestamp":1700000000001,"tokenEstimate":1,"message":{"role":"user","content":"alpha"}}"#,
            "\n",
            r#"{"type":"message","id":"00000001","parentId":"00000000","timestamp":1700000000002,"tokenEstimate":1,"message":{"role":"assistant","content":"beta"}}"#,
            "\n",
            // A compaction entry as written before ADR-0044: no carryPaths key.
            r#"{"type":"compaction","id":"00000002","parentId":"00000001","timestamp":1700000000003,"createdAt":1700000000003,"coveredFrom":"00000000","coveredTo":"00000001","summary":"OLD SUMMARY","tokenEstimate":3}"#,
            "\n",
        );
        fs::write(&path, legacy).unwrap();

        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), "nocarry1");
        assert_eq!(contents(&session), ["OLD SUMMARY"]);
    }

    #[test]
    fn rebuild_applies_multiple_non_overlapping_compactions() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let a = log.append(&Message::user("a")).unwrap();
        log.append(&Message::assistant("b")).unwrap();
        let c = log.append(&Message::user("c")).unwrap();
        log.append(&Message::assistant("d")).unwrap();
        log.append_compaction(&a, &a, "S1", &[], None).unwrap();
        log.append_compaction(&c, &c, "S2", &[], None).unwrap();
        drop(log);

        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        // Each covered id is replaced independently and deterministically.
        assert_eq!(contents(&session), ["S1", "b", "S2", "d"]);
    }

    #[test]
    fn rebuild_rejects_overlapping_compaction_coverage() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let a = log.append(&Message::user("a")).unwrap();
        let b = log.append(&Message::assistant("b")).unwrap();
        let c = log.append(&Message::user("c")).unwrap();
        // Two compactions whose covered ranges overlap on `b`.
        log.append_compaction(&a, &b, "S1", &[], None).unwrap();
        log.append_compaction(&b, &c, "S2", &[], None).unwrap();
        drop(log);

        let store = SessionStore::with_root(dir.path.clone());
        let meta = store.find(&id).unwrap().unwrap();
        assert!(
            store.open(&meta).is_err(),
            "overlapping compaction coverage must be rejected, not silently merged"
        );
    }

    #[test]
    fn rebuild_rejects_compaction_covering_an_unknown_id() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        log.append(&Message::user("a")).unwrap();
        // `ffffffff` is not an entry id in this session.
        log.append_compaction("ffffffff", "ffffffff", "S", &[], None)
            .unwrap();
        drop(log);

        let store = SessionStore::with_root(dir.path.clone());
        let meta = store.find(&id).unwrap().unwrap();
        assert!(store.open(&meta).is_err());
    }

    #[test]
    fn resume_continues_the_chain_after_a_compaction_entry() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let from = log.append(&Message::user("a")).unwrap();
        let to = log.append(&Message::assistant("b")).unwrap();
        let compaction_id = log.append_compaction(&from, &to, "SUM", &[], None).unwrap();
        let path = log.path().to_path_buf();
        drop(log);

        // Resuming must treat the compaction entry as the current leaf: the next
        // append links onto it and draws a fresh, non-colliding id.
        let mut resumed = SessionLog::resume(&path).unwrap();
        let new_id = resumed.append(&Message::user("c")).unwrap();
        drop(resumed);
        assert_ne!(
            new_id, compaction_id,
            "resumed id must not collide with the compaction id"
        );
        let last = lines(&path).pop().unwrap();
        assert_eq!(last["message"]["content"], "c");
        assert_eq!(last["parentId"], compaction_id);

        // And the rebuilt context is summary + the post-compaction turn.
        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        assert_eq!(contents(&session), ["SUM", "c"]);
    }

    #[test]
    fn estimate_tokens_is_conservative_and_nonzero_for_short_content() {
        assert_eq!(estimate_tokens(""), 0);
        // Rounds up: 1..=4 chars -> 1 token, never truncating to 0.
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn append_persists_a_per_message_token_estimate() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        log.append(&Message::user("hello")).unwrap();
        let entry = &lines(log.path())[1];
        // The persisted per-turn token accounting the foundation records.
        assert_eq!(entry["tokenEstimate"], json!(estimate_tokens("hello")));
    }

    #[test]
    fn append_persists_optional_provider_turn_id_and_legacy_entries_still_read() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        log.append(&Message::assistant("hi").with_provider_turn_id("turn_00000000"))
            .unwrap();
        let path = log.path().to_path_buf();
        drop(log);

        let entry = &lines(&path)[1];
        assert_eq!(entry["providerTurnId"], "turn_00000000");

        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        assert_eq!(
            session.messages[0].provider_turn_id.as_deref(),
            Some("turn_00000000")
        );

        let cwd_dir = dir.path.join("legacy");
        fs::create_dir(&cwd_dir).unwrap();
        let legacy_path = cwd_dir.join("legacy.jsonl");
        let legacy = concat!(
            r#"{"type":"session","version":2,"id":"legacy123","timestamp":1700000000000,"cwd":"/legacy"}"#,
            "\n",
            r#"{"type":"message","id":"00000000","parentId":null,"timestamp":1700000000001,"tokenEstimate":1,"message":{"role":"assistant","content":"old"}}"#,
            "\n",
        );
        fs::write(&legacy_path, legacy).unwrap();
        let legacy_session = open_by_id(&SessionStore::with_root(dir.path.clone()), "legacy123");
        assert_eq!(legacy_session.messages[0].provider_turn_id, None);
    }

    #[test]
    fn rebuilt_context_reports_the_same_token_total_across_resume() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let messages = [
            Message::user("explain the build"),
            Message::assistant("it runs cargo build then the gate"),
            Message::user("and the tests?"),
        ];
        for message in &messages {
            log.append(message).unwrap();
        }
        drop(log);

        // "before": the live in-session total computed from the same messages.
        let live_total = total(&messages);
        assert!(live_total > 0, "non-empty context must count > 0 tokens");

        // "after": reopen the persisted session and read the rebuilt total.
        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        assert_eq!(
            session.context_tokens, live_total,
            "reopened context token total must match the live total"
        );

        // Stable on a second reopen too (deterministic, not order/time sensitive).
        let reopened = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        assert_eq!(reopened.context_tokens, session.context_tokens);
    }

    #[test]
    fn compacted_total_counts_the_summary_not_the_covered_turns() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        let id = log.id().to_string();
        let from = log
            .append(&Message::user("a very long original first turn"))
            .unwrap();
        let to = log
            .append(&Message::assistant("an equally long original reply"))
            .unwrap();
        let tail = Message::user("short tail");
        log.append(&tail).unwrap();
        log.append_compaction(&from, &to, "sum", &[], None).unwrap();
        drop(log);

        let session = open_by_id(&SessionStore::with_root(dir.path.clone()), &id);
        // The rebuilt context is [summary, tail]; its total is exactly the sum
        // of those two, not the covered originals.
        let expected = total(&[Message::user("sum"), tail]);
        assert_eq!(session.context_tokens, expected);
        assert_eq!(session.context_tokens, total(&session.messages));
    }

    #[test]
    fn legacy_v1_session_without_token_estimates_still_reports_a_total() {
        // A pre-foundation transcript: no per-entry tokenEstimate field. The
        // read path must recompute from content so old sessions still report a
        // stable, non-zero total instead of breaking or reading as 0.
        let dir = temp_dir();
        let cwd_dir = dir.path.join("w");
        fs::create_dir(&cwd_dir).unwrap();
        let path = cwd_dir.join("v1.jsonl");
        let v1 = concat!(
            r#"{"type":"session","version":1,"id":"abcd1234","timestamp":1700000000000,"cwd":"/w"}"#,
            "\n",
            r#"{"type":"message","timestamp":1700000000001,"message":{"role":"user","content":"hello"}}"#,
            "\n",
            r#"{"type":"message","timestamp":1700000000002,"message":{"role":"assistant","content":"hi there"}}"#,
            "\n",
        );
        fs::write(&path, v1).unwrap();

        let store = SessionStore::with_root(dir.path.clone());
        let meta = store.find("abcd1234").unwrap().unwrap();
        let session = store.open(&meta).unwrap();
        let expected = total(&[Message::user("hello"), Message::assistant("hi there")]);
        assert_eq!(session.context_tokens, expected);
        assert!(session.context_tokens > 0);
    }

    fn meta_for(id: &str, cwd: &str, created_ms: u128) -> SessionMeta {
        SessionMeta {
            id: id.to_string(),
            path: PathBuf::from(format!("/tmp/{id}.jsonl")),
            cwd: cwd.to_string(),
            created_ms,
            updated_ms: created_ms,
        }
    }

    #[test]
    fn newest_for_cwd_picks_first_match_for_directory() {
        // list() returns newest-first, so the first matching cwd is the newest.
        let metas = vec![
            meta_for("newest-other", "/other", 300),
            meta_for("newest-here", "/here", 200),
            meta_for("older-here", "/here", 100),
        ];
        let picked = newest_for_cwd(&metas, "/here").expect("a session for /here");
        assert_eq!(picked.id, "newest-here");
        assert!(newest_for_cwd(&metas, "/absent").is_none());
    }

    #[test]
    fn preview_line_collapses_whitespace_and_truncates() {
        assert_eq!(
            preview_line("  hello   world\n\tagain "),
            "hello world again"
        );
        let long = "word ".repeat(40);
        let preview = preview_line(&long);
        assert!(preview.ends_with('…'), "{preview}");
        assert_eq!(preview.chars().count(), PREVIEW_CHARS + 1);
    }

    #[test]
    fn relative_age_buckets_by_magnitude() {
        let minute = 60_000u128;
        assert_eq!(relative_age(minute * 10, minute * 10), "just now");
        assert_eq!(relative_age(minute * 10, minute * 9), "1m ago");
        assert_eq!(relative_age(minute * 200, minute * 20), "3h ago");
        assert_eq!(relative_age(minute * 60 * 24 * 3, 0), "3d ago");
        // A future/malformed timestamp never underflows.
        assert_eq!(relative_age(0, minute * 5), "just now");
    }

    #[test]
    fn first_user_preview_stops_at_first_user_message() {
        let dir = temp_dir();
        let mut log = SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        log.append(&Message::assistant("system-ish first")).unwrap();
        log.append(&Message::user("please fix the   login\nbug"))
            .unwrap();
        log.append(&Message::user("second user turn")).unwrap();
        let preview = first_user_preview(log.path()).expect("a user message preview");
        assert_eq!(preview, "please fix the login bug");
    }

    #[test]
    fn resumable_for_cwd_filters_and_previews_newest_first() {
        let dir = temp_dir();
        let store = SessionStore::with_root(dir.path.clone());
        let mut here_old = SessionLog::create_in(&dir.path, Path::new("/proj")).unwrap();
        here_old.append(&Message::user("old task")).unwrap();
        // Ensure a distinct created timestamp so ordering is deterministic.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let mut here_new = SessionLog::create_in(&dir.path, Path::new("/proj")).unwrap();
        here_new.append(&Message::user("new task")).unwrap();
        let mut elsewhere = SessionLog::create_in(&dir.path, Path::new("/other")).unwrap();
        elsewhere.append(&Message::user("unrelated")).unwrap();

        let resumable = store.resumable_for_cwd("/proj").unwrap();
        assert_eq!(resumable.len(), 2, "only /proj sessions");
        assert_eq!(resumable[0].meta.id, here_new.id(), "newest first");
        assert_eq!(resumable[0].preview, "new task");
        assert_eq!(resumable[1].preview, "old task");
    }

    #[test]
    fn list_orders_by_recent_activity_not_creation_time() {
        let dir = temp_dir();
        let store = SessionStore::with_root(dir.path.clone());
        let mut older = SessionLog::create_in(&dir.path, Path::new("/proj")).unwrap();
        older.append(&Message::user("older created")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let newer = SessionLog::create_in(&dir.path, Path::new("/proj")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        older
            .append(&Message::assistant("recent activity"))
            .unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(sessions[0].id, older.id());
        assert_eq!(sessions[1].id, newer.id());
        assert!(sessions[0].created_ms < sessions[1].created_ms);
        assert!(sessions[0].updated_ms >= sessions[1].updated_ms);
    }
}
