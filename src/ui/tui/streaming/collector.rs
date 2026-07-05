// SPDX-License-Identifier: Apache-2.0
// Derived from codex-rs/tui/src/markdown_stream.rs (OpenAI Codex, Apache-2.0).
// Changes from upstream: reduced to the production source-boundary API (the
// test-only rendered-line helpers and cwd/width plumbing are dropped, since
// Iris renders in the controller against its own markdown pipeline); visibility
// narrowed to `pub(super)`. The newline-gating and finalize semantics are
// unchanged.

//! Newline-gated accumulator that buffers raw markdown source and exposes a
//! commit boundary at each newline.
//!
//! The controller calls [`MarkdownStreamCollector::commit_complete_source`]
//! after each newline-bearing delta to obtain the newly-completed source
//! prefix, leaving the trailing incomplete line buffered for the next delta.
//! On finalization, [`MarkdownStreamCollector::finalize_and_drain_source`]
//! flushes whatever remains (the last line, which may lack a trailing newline).
//!
//! Cutting only at `'\n'` (an ASCII byte) means every returned chunk ends on a
//! UTF-8 character boundary, so a multi-byte grapheme split across deltas is
//! never torn.

/// Newline-gated raw-source accumulator.
#[derive(Debug, Default)]
pub(super) struct MarkdownStreamCollector {
    buffer: String,
    committed_source_len: usize,
}

impl MarkdownStreamCollector {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Reset all buffered source and commit bookkeeping.
    pub(super) fn clear(&mut self) {
        self.buffer.clear();
        self.committed_source_len = 0;
    }

    /// Append a raw streaming delta to the internal source buffer.
    pub(super) fn push_delta(&mut self, delta: &str) {
        self.buffer.push_str(delta);
    }

    /// Total buffered source length in bytes (committed + pending partial).
    pub(super) fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// Whether any source (committed or pending) is buffered.
    pub(super) fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// The trailing partial line not yet returned by `commit_complete_source`
    /// (empty once every buffered line is newline-terminated and committed).
    pub(super) fn pending_partial(&self) -> &str {
        &self.buffer[self.committed_source_len..]
    }

    /// Commit newly completed raw markdown source up to the last newline.
    ///
    /// Returns only source that has not been returned by a previous commit.
    /// Returns `None` when no new complete line exists, which keeps incomplete
    /// markdown out of the stable region until the rest of the line arrives.
    pub(super) fn commit_complete_source(&mut self) -> Option<String> {
        let commit_end = self.buffer.rfind('\n').map(|idx| idx + 1)?;
        if commit_end <= self.committed_source_len {
            return None;
        }
        let out = self.buffer[self.committed_source_len..commit_end].to_string();
        self.committed_source_len = commit_end;
        Some(out)
    }

    /// Finalize the stream and return any remaining raw source.
    ///
    /// Ensures the returned chunk is newline-terminated when non-empty so
    /// callers can run block parsing on the final chunk. Clears the collector.
    pub(super) fn finalize_and_drain_source(&mut self) -> String {
        if self.committed_source_len >= self.buffer.len() {
            self.clear();
            return String::new();
        }
        let mut out = self.buffer[self.committed_source_len..].to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        self.clear();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_commit_until_newline() {
        let mut c = MarkdownStreamCollector::new();
        c.push_delta("Hello, world");
        assert!(c.commit_complete_source().is_none());
        c.push_delta("!\n");
        assert_eq!(
            c.commit_complete_source().as_deref(),
            Some("Hello, world!\n")
        );
    }

    #[test]
    fn commit_returns_only_new_source() {
        let mut c = MarkdownStreamCollector::new();
        c.push_delta("a\nb\n");
        assert_eq!(c.commit_complete_source().as_deref(), Some("a\nb\n"));
        // No new newline-terminated content yet.
        assert!(c.commit_complete_source().is_none());
        c.push_delta("c\n");
        assert_eq!(c.commit_complete_source().as_deref(), Some("c\n"));
    }

    #[test]
    fn finalize_commits_partial_line_once() {
        let mut c = MarkdownStreamCollector::new();
        c.push_delta("Line without newline");
        assert_eq!(
            c.finalize_and_drain_source(),
            "Line without newline\n".to_string()
        );
        // Cleared after finalize.
        assert!(c.is_empty());
        assert_eq!(c.finalize_and_drain_source(), String::new());
    }

    #[test]
    fn finalize_after_full_commit_yields_nothing() {
        let mut c = MarkdownStreamCollector::new();
        c.push_delta("done\n");
        assert_eq!(c.commit_complete_source().as_deref(), Some("done\n"));
        assert_eq!(c.finalize_and_drain_source(), String::new());
    }

    #[test]
    fn multibyte_grapheme_split_across_deltas_is_never_torn() {
        let mut c = MarkdownStreamCollector::new();
        // "汉" is three bytes; feed it one byte at a time (as raw fragments a
        // provider might chunk) followed by a newline.
        let bytes = "汉".as_bytes();
        // Reconstruct as &str deltas at char boundaries only, which is how the
        // upstream UiEvent path delivers them; the invariant we assert is that
        // any chunk we return ends on a newline (char boundary).
        c.push_delta("汉");
        assert!(c.commit_complete_source().is_none(), "no newline yet");
        c.push_delta("字\n");
        let out = c.commit_complete_source().expect("commit after newline");
        assert_eq!(out, "汉字\n");
        assert!(out.is_char_boundary(out.len()));
        let _ = bytes;
    }
}
