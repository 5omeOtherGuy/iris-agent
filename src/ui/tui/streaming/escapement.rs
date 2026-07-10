//! The escapement: a word-quantized drain buffer between delta *arrival* and
//! the visible tail.
//!
//! A mechanical escapement turns an irregular drive force into a steady tick.
//! This one turns raw network bursts into even, word-quantized beats on the
//! loop's tick grid: deltas [`push`](Escapement::push)ed as they arrive are
//! held in a tiny buffer and released one [`beat`](Escapement::beat) at a time,
//! each beat a baseline byte quantum extended to the next word boundary. The
//! buffer is TINY by construction — it shapes rhythm, it does not hold content
//! back — and any consumer (the assistant active tail, the reasoning stream)
//! drives it from the SAME cadence that drives `commit_stream_tick`; there is
//! never a second timer, so `beat` takes the tick's `now` only to document that
//! it is a per-tick call (the quantum itself is size-based, not time-based).
//!
//! Reduced motion is a no-op pass-through and is handled by the callers, which
//! bypass the escapement entirely (arrival renders immediately) — see
//! `controller`'s `passthrough` and the transcript's reasoning path.

/// Baseline drain floor per beat, in bytes. Below this the whole buffer fits in
/// one beat; at or above it the drain is `ceil(pending_len / 2)`. The `24`-byte
/// floor is what makes a steady-state backlog (≤ ~2 beats of drain) empty in
/// ≤ 2 beats once arrival stops.
const QUANTUM_MIN_BYTES: usize = 24;

/// Word-quantized drain buffer between delta arrival and the visible tail.
#[derive(Debug, Default)]
pub(crate) struct Escapement {
    /// Arrived, not yet released to the visible tail. Everything the caller has
    /// already taken (via `beat`/`flush`) is gone from here.
    pending: String,
}

impl Escapement {
    /// Append an arrival. Cheap: no policy runs here, the beat owns the rhythm.
    pub(crate) fn push(&mut self, delta: &str) {
        self.pending.push_str(delta);
    }

    /// Release the next quantum to append to the visible tail, or `None` when
    /// empty. Called once per loop tick (the same beat that drives
    /// `commit_stream_tick`); `_now` is unused because the quantum is size-based
    /// — the cadence is the caller's single tick, never a timer in here.
    pub(crate) fn beat(&mut self, _now: std::time::Instant) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let end = quantum_end(&self.pending);
        Some(self.take(end))
    }

    /// Release everything immediately (stream end, cancel/error, approval gate,
    /// session reset, entering reduced motion). After a flush the buffer is
    /// empty and stays pass-through until the next push.
    pub(crate) fn flush(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.pending))
    }

    /// Whether anything is buffered (drives the loop's "has stream work" gate so
    /// held text is always beaten out, never stranded).
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// The still-buffered text (for a caller that needs to inspect the tail,
    /// e.g. the reasoning section-break de-dup).
    pub(crate) fn pending(&self) -> &str {
        &self.pending
    }

    /// Discard all buffered text (session reset / after a consumer's own reset).
    pub(crate) fn clear(&mut self) {
        self.pending.clear();
    }

    /// Split off `end` bytes from the front of `pending` (assumes `end` is a
    /// char boundary, which `quantum_end` guarantees).
    fn take(&mut self, end: usize) -> String {
        let rest = self.pending.split_off(end);
        std::mem::replace(&mut self.pending, rest)
    }
}

/// The byte length of the next beat's quantum from `pending` (non-empty).
///
/// Baseline `max(ceil(pending_len / 2), 24)` bytes, snapped up to a UTF-8 char
/// boundary, then extended to the next word boundary so a word is never split:
/// finish the word straddling the cut, then let the trailing whitespace run
/// ride with it. When there is no whitespace at or after the cut (CJK, or a long
/// unbroken token still arriving) there is no word boundary to honour, so it
/// falls back to the char-boundary-snapped baseline — bounded, never the whole
/// buffer. A newline is just whitespace here: it rides through and hands off to
/// the line pipeline downstream (it is never held back).
fn quantum_end(pending: &str) -> usize {
    let n = pending.len();
    let baseline = (n.div_ceil(2)).max(QUANTUM_MIN_BYTES);
    if baseline >= n {
        return n;
    }
    // Never split a UTF-8 sequence: snap the byte cut up to a char boundary.
    let cut = pending.ceil_char_boundary(baseline);
    if cut >= n {
        return n;
    }
    match pending[cut..].find(char::is_whitespace) {
        // No word boundary ahead: the no-whitespace fallback drains at the
        // char-snapped baseline (keeps CJK/long tokens bounded).
        None => cut,
        Some(off) => {
            let word_end = cut + off;
            // Whitespace runs ride with the word before them: extend across the
            // run to the start of the next word (or the buffer end).
            match pending[word_end..].find(|c: char| !c.is_whitespace()) {
                Some(next) => word_end + next,
                None => n,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn now() -> Instant {
        Instant::now()
    }

    /// Drain to empty and return the number of beats it took, asserting the
    /// concatenation of beats reproduces the input exactly (nothing lost/dupe'd)
    /// and no beat ever splits a UTF-8 char.
    fn drain_beats(input: &str) -> (usize, String) {
        let mut esc = Escapement::default();
        esc.push(input);
        let mut out = String::new();
        let mut beats = 0;
        while let Some(q) = esc.beat(now()) {
            assert!(
                q.is_char_boundary(0) && input.is_char_boundary(out.len() + q.len()),
                "beat split a UTF-8 char at {:?}",
                q
            );
            out.push_str(&q);
            beats += 1;
            assert!(beats < 10_000, "beat did not converge");
        }
        (beats, out)
    }

    #[test]
    fn small_buffer_drains_whole_in_one_beat() {
        // Under the 24-byte floor the whole buffer releases at once.
        let (beats, out) = drain_beats("Hello");
        assert_eq!(out, "Hello");
        assert_eq!(beats, 1);
    }

    #[test]
    fn empty_beat_and_flush_return_none() {
        let mut esc = Escapement::default();
        assert!(esc.beat(now()).is_none());
        assert!(esc.flush().is_none());
        assert!(esc.is_empty());
    }

    #[test]
    fn flush_returns_everything() {
        let mut esc = Escapement::default();
        esc.push("some pending text that has not been shown yet");
        let all = esc.flush().expect("flush yields the buffer");
        assert_eq!(all, "some pending text that has not been shown yet");
        assert!(esc.is_empty());
        assert!(esc.flush().is_none(), "flush is idempotent when empty");
    }

    #[test]
    fn beat_never_splits_a_word() {
        let mut esc = Escapement::default();
        // 60 bytes of real words: the first beat drains ceil(60/2)=30 bytes,
        // extended forward to the next space — never mid-word.
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india";
        esc.push(text);
        let first = esc.beat(now()).expect("a beat");
        assert!(
            first.len() >= 30,
            "beat drains at least the baseline quantum: {} bytes",
            first.len()
        );
        // The released chunk ends on a word boundary (whitespace or the buffer
        // end), so the remainder starts a fresh word.
        assert!(
            first.ends_with(char::is_whitespace) || esc.is_empty(),
            "released chunk ends on a word boundary: {first:?}"
        );
        let mut all = first;
        while let Some(q) = esc.beat(now()) {
            all.push_str(&q);
        }
        assert_eq!(all, text, "beats reproduce the input exactly");
    }

    #[test]
    fn baseline_quantum_is_half_rounded_up() {
        // 100 bytes with no spaces until the very end forces the fallback: the
        // first beat drains exactly ceil(100/2) = 50 bytes (char-snapped).
        let text = "x".repeat(100);
        let mut esc = Escapement::default();
        esc.push(&text);
        let first = esc.beat(now()).expect("a beat");
        assert_eq!(first.len(), 50, "ceil(100/2) baseline for a no-space run");
    }

    #[test]
    fn convergence_empties_a_steady_state_backlog_in_two_beats() {
        // A steady-state backlog under modest arrival stays ≤ ~2 beats of drain
        // (≤ 48 bytes here); once arrival stops it empties in ≤ 2 beats.
        for n in [25usize, 30, 40, 48] {
            let text: String = (0..n).map(|i| (b'a' + (i % 26) as u8) as char).collect();
            let (beats, out) = drain_beats(&text);
            assert_eq!(out, text);
            assert!(
                beats <= 2,
                "a {n}-byte backlog must empty in ≤ 2 beats, took {beats}"
            );
        }
    }

    #[test]
    fn sustained_arrival_keeps_the_backlog_bounded() {
        // Feed a token-sized delta every beat and drain once per beat: the held
        // buffer never grows past ~2 beats of drain — the escapement shapes
        // rhythm, it does not accumulate content.
        let mut esc = Escapement::default();
        let delta = "token12 "; // 8 bytes/beat, a realistic delta size
        let mut peak = 0usize;
        for _ in 0..200 {
            esc.push(delta);
            peak = peak.max(esc.pending().len());
            let _ = esc.beat(now());
        }
        assert!(
            peak <= 2 * QUANTUM_MIN_BYTES,
            "sustained-arrival backlog stayed bounded (peak {peak} bytes)"
        );
        // Arrival stops: the residue empties within two beats.
        let mut beats = 0;
        while esc.beat(now()).is_some() {
            beats += 1;
        }
        assert!(beats <= 2, "residue empties in ≤ 2 beats, took {beats}");
    }

    #[test]
    fn large_burst_advances_by_the_quantum_never_the_whole_burst() {
        // 500 bytes in one push: the first beat releases only ~half (bounded),
        // and the buffer converges geometrically — it never bursts out at once.
        let text: String = (0..500).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let mut esc = Escapement::default();
        esc.push(&text);
        let first = esc.beat(now()).expect("a beat");
        assert!(
            first.len() < text.len(),
            "the first beat never releases the whole burst"
        );
        assert!(
            (250..=280).contains(&first.len()),
            "the first beat releases about half (got {})",
            first.len()
        );
        // The rest still drains fully, in order.
        let mut all = first;
        while let Some(q) = esc.beat(now()) {
            all.push_str(&q);
        }
        assert_eq!(all, text);
    }

    #[test]
    fn utf8_multibyte_never_torn() {
        // Accented Latin (2-byte chars) with spaces: word boundaries hold and no
        // char is split across beats.
        let text = "café résumé naïve façade jalapeño piñata über schön";
        let (_, out) = drain_beats(text);
        assert_eq!(out, text);
    }

    #[test]
    fn cjk_no_word_boundaries_drains_within_the_byte_bound() {
        // CJK has no ASCII spaces: every beat falls back to the char-snapped
        // byte quantum, so it stays bounded (never one giant burst) and never
        // splits a 3-byte char.
        let text = "汉字测试内容一二三四五六七八九十百千万亿兆京垓"; // 3 bytes each
        let bytes = text.len();
        let mut esc = Escapement::default();
        esc.push(text);
        let first = esc.beat(now()).expect("a beat");
        assert!(
            first.len() <= bytes / 2 + 3,
            "bounded to ~half, char-aligned"
        );
        assert!(!first.is_empty());
        let mut all = first;
        while let Some(q) = esc.beat(now()) {
            all.push_str(&q);
        }
        assert_eq!(all, text, "all CJK drained intact");
    }

    #[test]
    fn newline_rides_through_freely() {
        // A newline is just whitespace here: it is released, never held back
        // waiting for a later word boundary.
        let mut esc = Escapement::default();
        esc.push("First paragraph line.\n\nSecond paragraph that keeps going on.");
        let mut all = String::new();
        while let Some(q) = esc.beat(now()) {
            all.push_str(&q);
        }
        assert_eq!(
            all,
            "First paragraph line.\n\nSecond paragraph that keeps going on."
        );
    }
}
