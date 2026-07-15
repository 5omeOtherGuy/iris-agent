//! Tier-2 store for oversized tool outputs (issue #61).
//!
//! Large tool outputs (big shell logs, wide greps) bloat provider context if
//! they are reinserted into every round-trip. This store keeps the full output
//! out of context: the harness writes it here, and the transcript carries only a
//! compact preview plus a stable handle the full output can be retrieved by.
//!
//! It is the Tier-2 implementation of the Tier-1 [`ToolOutputStore`] contract;
//! Nexus owns the contract and the threshold/compaction policy and never touches
//! the filesystem itself (mirroring how `ApprovalGate`/`ChatProvider` are core
//! contracts implemented in the tiers above).
//!
//! Storage reuses the session-file location pattern: each session's handles live
//! in a sibling directory next to its JSONL transcript
//! (`<...>_<id>.jsonl` -> `<...>_<id>.outputs/`), so they share the session's
//! lifetime and cleanup. Handles are content-addressed (a truncated SHA-256 of
//! the output), so identical output stores once and the same logical output maps
//! to the same handle across a resume -- the reference written into the
//! transcript stays valid without any per-session counter state.
//!
//! Deliberately out of scope (issue #61): cloud/blob storage, a database,
//! search/indexing, binary artifacts, and a model-facing dereference tool or TUI
//! browser. This is durable local storage plus stable handles -- the foundation
//! a later selective-dereference slice reads, not the consumer itself.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::nexus::ToolOutputStore;

/// Number of leading SHA-256 bytes used for a handle id. 16 bytes (128 bits,
/// 32 hex chars) is collision-resistant far beyond per-session output volume
/// and keeps the on-disk filename short.
const HANDLE_ID_BYTES: usize = 16;

/// Local, session-scoped store for oversized tool outputs. Holds only the target
/// directory; the directory is created lazily on first write so a session that
/// never offloads leaves nothing behind.
#[derive(Clone)]
pub(crate) struct HandleStore {
    dir: PathBuf,
}

impl HandleStore {
    /// Derive the store directory from the session transcript path, replacing
    /// the `.jsonl` extension with `.outputs` so handles sit beside the session
    /// they belong to.
    pub(crate) fn for_session(session_path: &Path) -> Self {
        Self {
            dir: session_path.with_extension("outputs"),
        }
    }

    /// Construct a store at an explicit directory (tests).
    #[cfg(test)]
    pub(crate) fn with_dir(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl ToolOutputStore for HandleStore {
    /// Read a stored output back by handle id, returning `None` when no such
    /// handle exists. This is the retrieval half of "the full output stays
    /// retrievable by handle"; the model-facing `read_output` dereference tool
    /// (issue #205) calls it through the [`ToolOutputStore`] contract.
    ///
    /// The id is validated as hex-only before use: although handles are minted
    /// by [`handle_id`], a resumed transcript's reference is untrusted input, so
    /// a forged id containing path separators or `..` must never escape `dir`.
    fn get(&self, id: &str) -> Result<Option<String>> {
        if !is_handle_id(id) {
            return Ok(None);
        }
        let path = self.dir.join(format!("{id}.txt"));
        match fs::read_to_string(&path) {
            Ok(content) => Ok(Some(content)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("failed to read handle {}", path.display()))
            }
        }
    }

    /// Persist the full output and return its content-addressed handle id.
    /// Identical content yields the same id and is stored once (idempotent), so
    /// a resumed session that re-derives the same output reuses the same file.
    fn put(&self, content: &str) -> Result<String> {
        let id = handle_id(content);
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create handle dir {}", self.dir.display()))?;
        let path = self.dir.join(format!("{id}.txt"));
        // Content-addressed: an existing file already holds exactly this content.
        if path.exists() {
            return Ok(id);
        }
        // Write to a temp file then rename so a crash mid-write never leaves a
        // truncated file at the content-addressed path (which `exists()` would
        // then trust). Puts within a session are sequential, so a fixed temp
        // name per id cannot race itself.
        let temp = self.dir.join(format!("{id}.tmp"));
        fs::write(&temp, content)
            .with_context(|| format!("failed to write handle {}", temp.display()))?;
        fs::rename(&temp, &path)
            .with_context(|| format!("failed to finalize handle {}", path.display()))?;
        Ok(id)
    }
}

/// Content-addressed handle id: the hex of the leading [`HANDLE_ID_BYTES`] of
/// the output's SHA-256. Deterministic, so the same output always maps to the
/// same handle (stable across resume, dedup within a session).
fn handle_id(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    let mut id = String::with_capacity(HANDLE_ID_BYTES * 2);
    for byte in &digest[..HANDLE_ID_BYTES] {
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// Whether `id` is a well-formed handle id: a non-empty hex string. Rejects path
/// separators, `..`, and any other non-hex character so an untrusted id from a
/// persisted transcript cannot traverse out of the store directory.
fn is_handle_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

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
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("iris-handles-test-{nanos}-{seq}"));
        fs::create_dir(&path).unwrap();
        TempDir { path }
    }

    #[test]
    fn put_then_get_round_trips_the_full_output() {
        let dir = temp_dir();
        let store = HandleStore::with_dir(dir.path.join("outputs"));
        let big = "x".repeat(100_000);
        let id = store.put(&big).unwrap();
        assert_eq!(store.get(&id).unwrap().as_deref(), Some(big.as_str()));
    }

    #[test]
    fn identical_content_is_stored_once_with_a_stable_id() {
        let dir = temp_dir();
        let store = HandleStore::with_dir(dir.path.join("outputs"));
        let id1 = store.put("same content").unwrap();
        let id2 = store.put("same content").unwrap();
        assert_eq!(id1, id2, "content-addressed id must be stable");

        let files: Vec<_> = fs::read_dir(dir.path.join("outputs"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("txt"))
            .collect();
        assert_eq!(files.len(), 1, "identical content must dedup to one file");
    }

    #[test]
    fn different_content_yields_different_handles() {
        let dir = temp_dir();
        let store = HandleStore::with_dir(dir.path.join("outputs"));
        assert_ne!(store.put("alpha").unwrap(), store.put("beta").unwrap());
    }

    #[test]
    fn get_returns_none_for_unknown_and_malformed_ids() {
        let dir = temp_dir();
        let store = HandleStore::with_dir(dir.path.join("outputs"));
        // Unknown but well-formed id.
        assert!(store.get("deadbeef").unwrap().is_none());
        // Forged ids with traversal characters are rejected, never read.
        assert!(store.get("../secret").unwrap().is_none());
        assert!(store.get("a/b").unwrap().is_none());
        assert!(store.get("").unwrap().is_none());
    }

    #[test]
    fn for_session_derives_a_sibling_outputs_dir() {
        let store = HandleStore::for_session(Path::new("/s/170_abcd1234.jsonl"));
        assert_eq!(store.dir, PathBuf::from("/s/170_abcd1234.outputs"));
    }
}
