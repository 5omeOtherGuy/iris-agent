//! The escapement: a word-quantized drain buffer between delta *arrival* and
//! the visible tail.
//!
//! A mechanical escapement turns an irregular drive force into a steady tick.
//! This one turns raw network bursts into even, word-quantized beats on the
//! loop's tick grid: deltas [`push`](Escapement::push)ed as they arrive are
//! held in a tiny buffer and released one [`beat`](Escapement::beat) at a time.
//! Each beat drains a fixed SHARE of the backlog (`pending / LAG_BEATS`,
//! clamped between about one word and about five), extended to the next word
//! boundary — so the cadence tracks arrival like a hand at the keys: it speeds
//! up when the stream runs hot, eases off as it thins, and never gulps a
//! sentence. The buffer is TINY by construction — it shapes rhythm, it does
//! not hold content back — and any consumer (the assistant active tail, the
//! reasoning stream) drives it from the SAME cadence that drives
//! `commit_stream_tick`; there is
//! never a second timer, so `beat` takes the tick's `now` only to document that
//! it is a per-tick call (the quantum itself is size-based, not time-based).
//!
//! Reduced motion is a no-op pass-through and is handled by the callers, which
//! bypass the escapement entirely (arrival renders immediately) — see
//! `controller`'s `passthrough` and the transcript's reasoning path.

/// The reserve the governor holds, in beats: each beat drains
/// `pending / LAG_BEATS`, so at steady state the visible tail runs about this
/// many ticks (~400 ms) behind arrival — close enough to feel live, deep
/// enough to absorb network burstiness without gulping.
const LAG_BEATS: usize = 4;

/// Drain floor per beat, in bytes — about one short word with its space. The
/// tail never stalls while text is pending, and the last trickle of a backlog
/// still reads as typing, not as a stuck machine.
const QUANTUM_MIN_BYTES: usize = 6;

/// Smooth ceiling per beat, in bytes — four or five words (~320 bytes/s at the
/// 100 ms tick), the fastest the tail moves while still reading as flow rather
/// than as pasted chunks.
const QUANTUM_MAX_BYTES: usize = 32;

/// Above this backlog, smoothness yields to convergence: the beat drains half
/// the buffer, so a pathological burst (a whole code block in one delta)
/// fast-forwards in a few beats instead of scrolling for tens of seconds.
const FIREHOSE_BYTES: usize = 1024;

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
/// The governor: a beat's share of the backlog (`pending / LAG_BEATS`),
/// clamped to `[QUANTUM_MIN_BYTES, QUANTUM_MAX_BYTES]` — proportional to
/// arrival, so the cadence speeds up under load and eases off as the stream
/// thins, floored so the tail never stalls, capped so it never pastes chunks.
/// Past `FIREHOSE_BYTES` the cap yields and the beat drains half the buffer
/// (bounded convergence beats smoothness on pathological bursts). The result
/// is snapped up to a UTF-8 char boundary, then extended to the next word
/// boundary so a word is never split: finish the word straddling the cut, then
/// let the trailing whitespace run ride with it. When there is no whitespace at
/// or after the cut (CJK, or a long unbroken token still arriving) there is no
/// word boundary to honour, so it falls back to the char-boundary-snapped
/// share — bounded, never the whole buffer. A newline is just whitespace here:
/// it rides through and hands off to the line pipeline downstream (it is never
/// held back).
fn quantum_end(pending: &str) -> usize {
    let n = pending.len();
    let baseline = if n > FIREHOSE_BYTES {
        n / 2
    } else {
        (n / LAG_BEATS).clamp(QUANTUM_MIN_BYTES, QUANTUM_MAX_BYTES)
    };
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
        // Under the drain floor the whole buffer releases at once.
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
        // Real words: the first beat drains its share of the backlog, extended
        // forward to the next space — never mid-word.
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india";
        esc.push(text);
        let first = esc.beat(now()).expect("a beat");
        assert!(
            first.len() >= QUANTUM_MIN_BYTES,
            "beat drains at least the floor: {} bytes",
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
    fn no_space_run_drains_at_the_governed_share() {
        // 100 bytes with no spaces forces the fallback: the first beat drains
        // exactly its share of the backlog, 100 / LAG_BEATS = 25 bytes.
        let text = "x".repeat(100);
        let mut esc = Escapement::default();
        esc.push(&text);
        let first = esc.beat(now()).expect("a beat");
        assert_eq!(first.len(), 25, "a beat's share of a 100-byte no-space run");
        // 200 bytes hits the smooth ceiling: the share (50) is capped at
        // QUANTUM_MAX_BYTES so the tail never pastes chunks.
        let text = "x".repeat(200);
        let mut esc = Escapement::default();
        esc.push(&text);
        let first = esc.beat(now()).expect("a beat");
        assert_eq!(first.len(), QUANTUM_MAX_BYTES, "the share is capped");
    }

    #[test]
    fn backlog_decays_smoothly_and_empties_after_arrival_stops() {
        // Once arrival stops, the governor eases off: successive quanta never
        // grow (the hand slows as the dictation thins), never exceed the smooth
        // ceiling, and the backlog empties within a bounded number of beats.
        for n in [25usize, 30, 40, 48, 64] {
            let text: String = (0..n).map(|i| (b'a' + (i % 26) as u8) as char).collect();
            let mut esc = Escapement::default();
            esc.push(&text);
            let mut quanta = Vec::new();
            while let Some(q) = esc.beat(now()) {
                quanta.push(q.len());
                assert!(quanta.len() < 64, "converges");
            }
            assert_eq!(quanta.iter().sum::<usize>(), n, "nothing lost");
            assert!(
                quanta.windows(2).all(|w| w[0] >= w[1]),
                "quanta ease off monotonically: {quanta:?}"
            );
            assert!(
                quanta.iter().all(|&q| q <= QUANTUM_MAX_BYTES),
                "no chunk past the smooth ceiling: {quanta:?}"
            );
            assert!(
                quanta.len() <= n / QUANTUM_MIN_BYTES + 2,
                "a {n}-byte backlog empties promptly, took {} beats",
                quanta.len()
            );
        }
    }

    #[test]
    fn sustained_arrival_keeps_the_backlog_bounded() {
        // Feed a token-sized delta every beat and drain once per beat: the held
        // buffer never grows past a couple of deltas — the escapement shapes
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
            peak <= 2 * delta.len(),
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
        // 500 bytes in one push: every beat stays at the smooth ceiling — the
        // burst scrolls evenly instead of pasting, and drains fully in order.
        let text: String = (0..500).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let mut esc = Escapement::default();
        esc.push(&text);
        let first = esc.beat(now()).expect("a beat");
        assert_eq!(
            first.len(),
            QUANTUM_MAX_BYTES,
            "the first beat holds the smooth ceiling"
        );
        // The rest still drains fully, in order, never past the ceiling.
        let mut all = first;
        while let Some(q) = esc.beat(now()) {
            assert!(q.len() <= QUANTUM_MAX_BYTES, "smooth throughout");
            all.push_str(&q);
        }
        assert_eq!(all, text);
    }

    #[test]
    fn firehose_backlog_fast_forwards_then_smooths() {
        // Past FIREHOSE_BYTES, convergence beats smoothness: the beat drains
        // half the buffer, then the governor resumes its smooth cadence once
        // the backlog is back in range.
        let text = "x".repeat(2 * FIREHOSE_BYTES);
        let mut esc = Escapement::default();
        esc.push(&text);
        let first = esc.beat(now()).expect("a beat");
        assert_eq!(first.len(), FIREHOSE_BYTES, "half the firehose at once");
        let second = esc.beat(now()).expect("a beat");
        assert_eq!(
            second.len(),
            QUANTUM_MAX_BYTES,
            "back in range, back to the smooth ceiling"
        );
    }

    #[test]
    fn cadence_tracks_arrival_speeding_up_and_easing_off() {
        // The rhythm promise: under hot arrival the quanta grow toward the
        // ceiling (the hand speeds up); once arrival stops they only shrink
        // (it eases off) down to the floor.
        let mut esc = Escapement::default();
        let mut hot_peak = 0usize;
        for _ in 0..20 {
            esc.push(&"x".repeat(24));
            if let Some(q) = esc.beat(now()) {
                hot_peak = hot_peak.max(q.len());
            }
        }
        assert!(
            hot_peak >= 20,
            "hot arrival speeds the cadence up (peak {hot_peak})"
        );
        let mut decay = Vec::new();
        while let Some(q) = esc.beat(now()) {
            decay.push(q.len());
            assert!(decay.len() < 64, "converges");
        }
        assert!(
            decay.windows(2).all(|w| w[0] >= w[1]),
            "the cadence eases off after arrival stops: {decay:?}"
        );
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
        let mut esc = Escapement::default();
        esc.push(text);
        let first = esc.beat(now()).expect("a beat");
        assert!(
            first.len() <= QUANTUM_MAX_BYTES + 3,
            "bounded to the smooth ceiling, char-aligned"
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
