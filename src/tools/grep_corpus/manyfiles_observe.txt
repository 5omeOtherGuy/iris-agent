//! Session-scoped file observation store for stale-file detection.
//!
//! Tracks the `{mtime, content_hash}` of every workspace file the agent has
//! read or written this session. Before mutating an *existing* file, `edit`
//! and `write` call [`ObservedFiles::ensure_fresh`]: if the file was never
//! observed, or its content changed since it was last seen, the mutation is
//! rejected so the agent re-reads before clobbering. Newly created files need
//! no prior observation (blind create is allowed, matching Claude Code).
//!
//! Staleness is decided by the content hash, not mtime: a same-mtime/changed-
//! hash file is still stale, and a changed-mtime/same-hash file is allowed
//! (its observation is refreshed). mtime is stored so a benign touch does not
//! invalidate the observation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

type ContentHash = [u8; 32];

struct Observation {
    mtime: Option<SystemTime>,
    hash: ContentHash,
}

/// Session-local map of canonical path -> last observed `{mtime, hash}`.
#[derive(Default)]
pub(crate) struct ObservedFiles {
    seen: HashMap<PathBuf, Observation>,
}

impl ObservedFiles {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record the current state of `path`, whose bytes are `content`. Called
    /// after a successful read/write/edit. The key is canonicalized so reads
    /// and mutations agree regardless of how each tool resolved the path.
    pub(crate) fn observe(&mut self, path: &Path, content: &[u8]) {
        self.seen.insert(
            key(path),
            Observation {
                mtime: mtime_of(path),
                hash: hash_of(content),
            },
        );
    }

    /// Preflight before mutating an existing file. `current` is the file's
    /// current on-disk bytes (already read by the caller). Rejects when the
    /// file was never observed this session or changed since last observed;
    /// refreshes the stored mtime on a benign (hash-equal) change.
    pub(crate) fn ensure_fresh(&mut self, path: &Path, current: &[u8]) -> Result<()> {
        let canonical = key(path);
        let current_hash = hash_of(current);
        let observed_mtime = match self.seen.get(&canonical) {
            None => bail!(
                "{} has not been read this session; read it before modifying it",
                path.display()
            ),
            Some(observed) if observed.hash != current_hash => bail!(
                "{} changed since it was last read; read it again before modifying it",
                path.display()
            ),
            Some(observed) => observed.mtime,
        };
        // hash matches, so the content is fresh. If only the mtime drifted
        // (e.g. a no-op touch), refresh the stored observation so the benign
        // change does not later look suspicious; otherwise leave it untouched.
        let current_mtime = mtime_of(path);
        if observed_mtime != current_mtime {
            self.seen.insert(
                canonical,
                Observation {
                    mtime: current_mtime,
                    hash: current_hash,
                },
            );
        }
        Ok(())
    }
}

fn key(path: &Path) -> PathBuf {
    // Canonicalize so the key matches across tools; fall back to the given
    // path if the file does not exist yet (e.g. observing a fresh write before
    // it lands is not a use we have, but stay total rather than panic).
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn hash_of(content: &[u8]) -> ContentHash {
    let mut hasher = Sha256::new();
    hasher.update(content);
    hasher.finalize().into()
}

fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::temp_dir;
    use std::fs;

    #[test]
    fn unobserved_existing_file_is_rejected() {
        let dir = temp_dir();
        let path = dir.path.join("f.txt");
        fs::write(&path, "hello").unwrap();

        let mut observed = ObservedFiles::new();
        let err = observed
            .ensure_fresh(&path, b"hello")
            .unwrap_err()
            .to_string();
        assert!(err.contains("has not been read this session"), "{err}");
    }

    #[test]
    fn observed_then_unchanged_is_fresh() {
        let dir = temp_dir();
        let path = dir.path.join("f.txt");
        fs::write(&path, "hello").unwrap();

        let mut observed = ObservedFiles::new();
        observed.observe(&path, b"hello");
        assert!(observed.ensure_fresh(&path, b"hello").is_ok());
    }

    #[test]
    fn changed_content_is_stale() {
        let dir = temp_dir();
        let path = dir.path.join("f.txt");
        fs::write(&path, "hello").unwrap();

        let mut observed = ObservedFiles::new();
        observed.observe(&path, b"hello");
        // Simulate the file changing on disk under the agent.
        let err = observed
            .ensure_fresh(&path, b"hello world")
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed since it was last read"), "{err}");
    }

    #[test]
    fn same_hash_different_mtime_is_allowed_and_refreshes() {
        let dir = temp_dir();
        let path = dir.path.join("f.txt");
        fs::write(&path, "hello").unwrap();

        let mut observed = ObservedFiles::new();
        observed.observe(&path, b"hello");
        // Touch the file: mtime advances but content (hash) is identical.
        fs::write(&path, "hello").unwrap();
        assert!(observed.ensure_fresh(&path, b"hello").is_ok());
        // Still fresh afterward (observation refreshed, not invalidated).
        assert!(observed.ensure_fresh(&path, b"hello").is_ok());
    }
}
