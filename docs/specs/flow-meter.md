# SPEC — The flow meter & the exhale

Status: approved for implementation · Surface: the working indicator (§7.7) and
the context meter's detent acknowledgment (§6 motion 4).
Complexity: MEDIUM. Iteration passes required after it works: at least 1.

---

## 0 · Goal

Give the running machine a real instrument: a **flow meter** on the working
indicator that shows, live, how hard the provider stream is flowing — with
sub-cell resolution and mechanical ballistics — and give the context meter an
**exhale**: a two-tick after-image when compaction reclaims capacity, the
symmetric twin of the existing new-LED flash.

Testable outcome: while a turn streams, the working indicator carries a
6-cell bar whose bright fill tracks the measured per-tick delta flow on a
fixed log scale with instant attack, quantized release, and a decaying dim
peak tick; it renders nothing when nothing flows, vanishes with the
indicator, and degrades per reduced motion. When the context meter's lit-LED
count drops, the vacated LEDs render as dim `●` for two ticks before settling
to `○`.

## 1 · Why this and not something else

The pane already tells you *that* the machine is working (LED chase) and *how
much it has consumed* (`↑177k ↓5.7k`, cumulative). It never shows **rate** —
the one thing an operator watches on a real instrument to know whether the
machine is idling, cruising, or saturated. A VU-style meter is the honest
form: it renders a measurement, not a mood. Terminals almost never do
sub-cell two-tone metering with peak-hold; iris doing it quietly, on one
line, inside an existing indicator, is the kind of advanced-but-disciplined
move this TUI is for.

## 2 · The meter

### 2.1 Placement

The working indicator's fixed live-meter segment, immediately after elapsed:

```
●···  1:27 ┊ ▊██▍·· ┊ reading files ┊ ↑177k ↓5.7k
```

The meter is **6 cells**. Keeping it next to the LED chase makes the two live
instruments read as one cluster and gives the meter a stable place.

### 2.2 What it measures (honesty clause)

**Display-stream inflow**: the byte length of streaming delta payloads as
they arrive in `Screen::apply` — assistant text deltas
(`UiEvent::AssistantTextDelta`), streamed reasoning deltas, and streamed tool
output chunks (enumerate every delta-bearing `UiEvent` variant in the code
and sample all of them; block-level, non-streamed events are NOT flow).
Accumulate bytes into a per-tick sampler; on each spinner tick, take the
accumulator as one sample. It measures what is genuinely arriving over the
wire into the pane — not our own commit pacing (`commit_stream_tick` is a
display choice and must NOT be the source), and not a fabricated tokens/sec
(usage arrives per provider round, too coarse). **The meter prints no
number.** An uncalibrated-unit meter that prints no unit lies about nothing;
the honest cumulative counters sit right beside it.

### 2.3 Scale and quantization

- Resolution: 6 cells × 8 eighths = **48 quanta** (level 0..=48).
- Fixed log scale: `level = round(48 · ln(1 + bytes) / ln(1 + FULL_SCALE))`
  with `FULL_SCALE: usize = 4096` bytes/tick (≈ 40 KB/s at the 100 ms tick —
  a saturated fast stream). Fixed calibration: the same inflow always reads
  the same. Clamp at 48; a sample of 0 is level 0.
- Rendering per cell (left to right, cell i covering quanta `8i+1..=8(i+1)`):
  - bright fill: the standard left-anchored partials
    ` ▏▎▍▌▋▊▉█` (U+2589–U+258F + full block) for however many of the cell's
    eighths are ≤ the display level; full cells are `█`;
  - unlit cells render a dim `·` — the **same unlit-cell mark the LED chase
    already uses** (`●···`), so the two instruments share one vocabulary;
  - the **peak tick**: the cell containing the peak quantum renders a dim `▏`
    instead of its `·` (only when the peak sits above the bright fill;
    a peak inside the bright fill is invisible, correctly).
- Bright fill uses the accent (ORANGE) style like the chase's lit LED; `·`
  and the peak tick are dim. Position/length is the signal — the meter passes
  the monochrome test with color removed.

### 2.4 Ballistics (quantized physics, §6)

Integer math on the tick grid; no easing curves:

- **Attack is instant**: `display = max(sample_level, display - RELEASE)` —
  a burst is never under-reported.
- **Release**: `RELEASE = 4` quanta per tick (a full-scale reading drains in
  ~1.2 s of silence).
- **Peak-hold**: `peak = max(peak, display)`; after a hold of 5 ticks with no
  new peak, it decays 1 quantum per tick. Peak never renders below the
  display level.
- Lifecycle: the sampler and meter live with the spinner. Rendered ONLY while
  the spinner is active (a looping/live motion must be genuinely live, §6).
  Reset all meter state when the spinner starts. During approval-wait the
  indicator is already hidden — no special casing.
- **Reduced motion** (`IRIS_REDUCED_MOTION` / the existing reduced-motion
  gate): no release ballistics, no peak tick — the bar renders the raw
  current sample level directly each tick. (Telemetry keeps updating —
  reduced motion removes physics, never data.)

## 3 · The exhale

Today the Detents system flashes a context-meter LED **bright** for two ticks
when it newly lights (§6 motion 4). Compaction is the opposite event — LEDs
go dark, capacity comes back — and today it happens with no acknowledgment.

- When the lit-LED count of the session-bar context meter **decreases**
  (compaction, microcompaction fold, `/clear`-adjacent flows — anything that
  honestly lowers usage), the LEDs that went dark render as **dim `●`** (the
  lit glyph at muted luminance — an after-image) for two ticks, then settle
  to `○`.
- Same tick grid, same 2-tick constant, same reduced-motion gate (settles
  instantly), same "armed only after first frame" rule as the flash — extend
  `Detents` in `src/ui/tui/screen.rs`; do not build a parallel system.
- Increase and decrease in the same tick: the increase's bright flash wins
  (news of growth outranks the echo of shrinkage).
- **Implementation reality (verified):** today the meter fill
  (`footer.context_used_tokens`) updates ONLY in the `ProviderTurnCompleted`
  branch (screen.rs ~1321–1350); `CompactionApplied`/`FoldApplied`
  (~1355–1373) adjust accounting but never the meter, so a reclaim currently
  repaints the meter a full turn late, silently. The exhale must fire AT the
  compaction/fold event: update the meter's used-tokens from the screen's
  compaction accounting at those events (derive the post-reclaim total from
  the fields those events carry; if only estimates exist, use them and say so
  in a comment — an honest estimate now beats an exact number a turn late),
  then arm the exhale on the darkening edge. The existing strictly-greater
  `advanced` gate stays for the bright flash.

## 4 · Design-language amendments (same change)

- §6 closed motion set: extend motion 4's meter clause to name both
  directions (bright flash on light-up, dim after-image on go-dark — "the
  exhale"), and add motion 5: **the flow meter** — instant-attack, quantized
  release and peak-decay, live only while the stream is. Keep the closed-set
  framing: this spec *amends* the set; nothing else may.
- §5 symbol vocabulary: add the partial-block ramp ` ▏▎▍▌▋▊▉█` (flow-meter
  fill), note `·` is shared by the chase and the meter as the unlit cell, and
  `▏` doubles as the peak tick (dim).
- §7.7: update the working indicator example line + one sentence on the
  meter and its truncation-drop position.

## 5 · Implementation notes

- New module or a contained section in `screen.rs` near the spinner: a
  `FlowMeter { accum: usize, display: u8, peak: u8, hold: u8 }` with
  `observe_bytes(usize)`, `tick()`, `spans() -> Vec<Span>`; unit-test it in
  isolation. Wire `observe_bytes` in `Screen::apply` on the delta events;
  `tick()` from the spinner tick path (NOT while `awaiting_approval` — ticks
  already stop there; do not change that).
- The indicator renderer (`working_indicator_line_with_activity`) appends the
  meter spans when a meter is provided; keep the function signature churn
  minimal and the tests' existing call sites compiling.
- `scripts/gate.sh` clean (pipefail if piped). Match house comment/test
  idiom.

## 6 · Acceptance criteria

1. Quantizer: monotonic in bytes; 0 → 0; ≥ FULL_SCALE → 48; fixed (no
   adaptive rescale).
2. Ballistics: a single burst to 48 then silence: display falls 48→44→40→…;
   peak holds 5 ticks then steps down 1/tick; display never exceeds burst;
   peak never below display.
3. Render: level 0 + peak 0 → six dim `·`; level 48 → six `█`; a mid level
   renders exactly one partial cell; peak above fill renders one dim `▏` in
   the correct cell, replacing that cell's `·`.
4. Lifecycle: meter absent when spinner inactive; state reset on spinner
   start; meter absent from the line while `awaiting_approval` (whole
   indicator already hidden — assert no regression).
5. Reduced motion: display equals the raw sample each tick; no peak tick
   rendered.
6. Truncation: at a width that can't fit the meter, the counters survive and
   the meter is dropped (assert both).
7. Exhale: meter lit-count 7→4 renders three dim `●` for exactly 2 ticks,
   then `○○○`; suppressed under reduced motion; simultaneous up+down favors
   the bright flash; not armed before the first settled frame.
8. Goldens: one working-indicator frame with the meter mid-fill + peak; the
   §7.7 doc example matches reality.
9. `bash scripts/gate.sh` passes.

## 7 · Out of scope

- Any numeric rate printout, adaptive calibration, or per-provider scaling.
- Meters anywhere else (statusline, session bar). One instrument, one home.
- Changing when ticks run (CPU-idle during approval stays).
