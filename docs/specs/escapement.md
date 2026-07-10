# SPEC — The escapement: even beats for the live stream

Status: approved for implementation · Surface: the streaming live tails
(assistant active tail, reasoning stream) — `src/ui/tui/streaming/*`,
transcript reasoning path, §6/§7.4 amendments.
Complexity: MEDIUM-HIGH. Iteration passes required after it works: 2.

---

## 0 · Goal

Decouple **arrival** from **display** for the live streaming tails with a
tiny, bounded buffer, so streamed text advances in even, word-quantized beats
on the tick grid instead of raw network bursts — the way a mechanical
escapement turns irregular drive force into a steady tick.

Testable outcome: while streaming, the visible tail never grows by a raw
burst; it advances by word-boundary quanta on the loop's tick cadence, with
governed, arrival-tracking cadence with ~400 ms steady-state lag; stream end
(or cancel, error, approval gate) flushes instantly; reduced motion disables
pacing entirely; and the committed-line pipeline (collector → table holdback
→ adaptive chunking) is byte-for-byte unaffected.

## 1 · What exists, and where the jank actually is (read first)

`src/ui/tui/streaming/mod.rs` documents the existing pipeline: raw deltas →
newline-gated `collector` → `table_holdback` → `controller` commits stable
LINES into scrollback, paced by `chunking.rs`'s `AdaptiveChunkingPolicy`
(Smooth = 1 line/commit-tick; CatchUp under queue pressure with hysteresis).
**Committed lines are already deliberate. Do not touch that pipeline's
behavior.**

The jank lives in the two places that pipeline does not govern:

1. **The mutable active tail** — the current incomplete line the controller
   keeps outside scrollback. It re-renders raw on every delta: network
   chunking makes it jump three words, stall, then jump fifteen.
2. **The reasoning stream** — raw thinking deltas (the recently landed
   Codex reasoning streaming) render on arrival with no pacing at all.
   Verify the exact render path in the transcript/thinking code and confirm
   whether it shares any of the streaming module; pace it with the same
   escapement either way.

Tool exec output (`MAX_EXEC_STREAM_BYTES` live cells) is deliberately OUT of
scope: a process's stdout bursts are its honest truth — machine output keeps
machine timing.

## 2 · The mechanism

One small, reusable component (suggested: `streaming/escapement.rs`):

```rust
/// Word-quantized drain buffer between delta arrival and the visible tail.
pub(super) struct Escapement {
    pending: String,      // arrived, not yet shown
    // visible text is whatever the caller has already taken
}
```

- `push(&mut self, delta: &str)` — append arrival (cheap, no policy).
- `beat(&mut self, now: Instant) -> Option<String>` — called on the loop's
  tick cadence (the same beat that drives `commit_stream_tick`; verify it is
  the 100 ms `TICK` grid in tui_loop.rs and use exactly that driver — ONE
  cadence for the whole stream area, never a second timer). Returns the next
  quantum to append to the visible tail, or None when empty.
- `flush(&mut self) -> Option<String>` — everything, immediately.

### 2.1 The beat quantum (the policy)

- Governed drain per beat (recalibrated 2026-07-10 for smooth human-rhythm
  flow, on user direction): a beat's share of the backlog —
  `pending_len / LAG_BEATS(=4)` bytes, clamped to `[6, 32]` (about one word
  to about five) — then **extended to the next word boundary** (never split
  a word or a UTF-8 sequence or an ANSI-significant unit mid-emit;
  whitespace runs ride with the word before them). Past `FIREHOSE(=1024)`
  bytes of backlog the cap yields and the beat drains half the buffer:
  bounded convergence beats smoothness on pathological bursts. Newlines pass
  through freely (a newline in the tail hands off to the line pipeline — see
  §2.3 interaction note).
- Rhythm properties (test them, don't just assert them in prose): the
  cadence is proportional to arrival — it speeds toward the ceiling under
  hot arrival and, once arrival stops, successive quanta only shrink down to
  the one-word floor (the hand eases off). Steady-state lag ≈ LAG_BEATS
  beats (~400 ms at the 100 ms grid); with arrival stopped a backlog drains
  at ≥ the floor per beat, monotonically easing. The buffer is TINY by
  construction — it shapes rhythm, it does not hold content back.
- No easing, no per-frame interpolation: the tail advances in discrete
  word-steps on the shared grid. Machines step (§6).

### 2.2 Flush triggers (the machine never withholds)

Instant full drain on: stream end (`AssistantTextEnd` / reasoning end),
provider turn completion/cancel/error, an approval gate opening
(`show_approval` — the user must review against complete context), session
reset/`/new`, and entering reduced motion. After a flush the escapement is
empty and stays pass-through until the next stream starts.

### 2.3 Integration points (verify before building)

- **Assistant tail**: the escapement sits between delta arrival and the
  controller's active-tail text. The collector/holdback/commit pipeline must
  see text WITH THE SAME timing it does today or earlier — never later than
  the visible tail (committed lines may never lag the tail they came from).
  The clean seam: escapement feeds BOTH the visible tail and the collector
  from the same drained output, so the whole downstream pipeline sees one
  consistent, paced source. If that seam proves wrong in the code, document
  the seam you chose and why; the invariant that matters is a single
  consistent text timeline for tail + commits.
- **Reasoning stream**: same component instance pattern, at whatever seam
  the thinking path renders deltas. The `▋` caret (§7.4) now advances in
  even word-steps — the visible print head.
- **Reduced motion** (`IRIS_REDUCED_MOTION` / existing gate): escapement is
  a no-op pass-through (arrival renders immediately). Pacing is motion;
  reduced motion gets the raw truth on arrival.
- **The flow meter spec (docs/specs/flow-meter.md)**, if already landed:
  the meter samples ARRIVAL (pre-escapement). Do not move its tap.

## 3 · Design-language amendments (same change)

- §6 motion set: add the escapement as a numbered motion — the live tail
  advances in word-quantized steps on the tick grid; governed cadence, ~400 ms steady-state lag;
  flush-on-finish list; reduced motion = pass-through.
- §7.4: one sentence — live reasoning text feeds through the escapement; the
  caret steps evenly.

## 4 · Acceptance criteria

1. Unit (Escapement): word-boundary integrity (never a split word/UTF-8
   char); governed-share quantum math (floor/ceiling/firehose); monotonic ease-off after arrival stops;
   sustained-arrival backlog bound; flush returns everything; UTF-8
   multi-byte and CJK (no word boundaries) still drain within the bound —
   define and test the no-whitespace fallback (drain at the byte quantum
   rounded to a char boundary).
2. Tail never bursts: feed 500 bytes in one delta → visible tail grows only
   by the beat quantum per tick until drained; committed lines unaffected
   (existing chunking tests still green, byte-identical goldens for a
   finished message).
3. Commit-pipeline consistency: with the escapement active, the finalized
   message (finish_stream) is byte-identical to the same deltas without the
   escapement — pacing changes WHEN, never WHAT.
4. Every flush trigger in §2.2 covered by a test (approval gate one
   included: pending text is visible before the REVIEW block renders).
5. Reasoning stream paced: a reasoning delta burst renders across beats; the
   caret advances at word boundaries; reasoning end flushes.
6. Reduced motion: pass-through (arrival == display in the same frame).
7. No second timer: the drain is driven by the existing tick/commit cadence
   (assert by construction/code review — name the driver in the PR).
8. `bash scripts/gate.sh` passes; §6/§7.4 amended.

## 5 · Out of scope

- The committed-line pipeline's policy (chunking.rs thresholds stay).
- Tool exec output pacing.
- Any sub-tick timer or per-frame interpolation.
- Working-indicator/telemetry pacing.
