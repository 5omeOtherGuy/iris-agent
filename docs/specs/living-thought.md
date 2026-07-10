# SPEC — The living thought: the thinking block while it thinks

Status: approved for implementation · Surface: the thinking/reasoning block
(§7.4), live-preview path in `src/ui/tui/transcript.rs`, header renderer in
`src/ui/tui/panel.rs`.
Complexity: MEDIUM-HIGH. Iteration passes required after it works: 2.

---

## 0 · Goal

While the model reasons, the thinking block must be a **live instrument
readout** — bounded in height, carrying its own telemetry, its leading edge
visible — instead of today's silent, unbounded, telemetry-less text flood.
Settled behavior (fold rules, splice-above-answer, redaction, telemetry
patch) is already right and must not regress.

Testable outcome: streaming reasoning renders as a **tail window** (last N
body rows) under a header that carries a lit lamp and live elapsed; the tail
line ends in the `▋` caret the design language already promises; a one-line
**hidden-amount row** makes the window honest; ctrl+o during streaming opens
the full live stream; on finish the block commits exactly as today.

## 1 · Grounding (verified against the code — trust these)

- Live reasoning today: an always-expanded transient preview, full body
  re-rendered as it grows (`live_reasoning_preview_rows`,
  `refresh_streaming_memo` in transcript.rs), header right field EMPTY, no
  pulse, no caret, no cap. All liveness signals live in the bottom working
  indicator only.
- Settled block: `▸/▾ THINKING` + `↓tokens elapsed` right field (patched in
  on `ProviderTurnCompleted` via `set_thinking_telemetry`), `┊` rail body,
  dim+italic markdown, fold rules per `has_distinct_expanded_body`,
  summary-only blocks not foldable, redacted never renders trace text.
- §7.4 of docs/TUI_DESIGN_LANGUAGE.md already PROMISES "Live reasoning
  pulses (`●` in the label, `▋` caret at the tail)" — the code never
  implemented it. Spec-vs-code conflicts resolve toward the spec; this
  change makes §7.4 true (with one amendment below: lamp, not pulse).

## 2 · The live state

### 2.1 Header — the lamp and the readout

While reasoning streams, the rail header renders:

```
▾ THINKING ●                                          14s
```

- `●` — a **static orange lamp** after the label: lit = receiving. NOT
  animated (the moving text and caret carry liveness; a second looping
  motion next to the LED chase would be noise). Drops when the block
  commits. Amend §7.4's word "pulses" to the lamp ("`●` lit in the label
  while receiving").
- Right field: **live elapsed** (`format_elapsed_compact` since this
  provider turn's reasoning began), updating on the tick grid — the live
  counterpart of the settled `↓2.4k 12s`. On commit, the existing patch
  path takes over unchanged. No fabricated token estimate: elapsed is the
  only number we truly have live; print nothing else.
- The label keeps its recessive muted-bold tone (the divergence from tool
  headers is intentional; do not "fix" it).

### 2.2 Body — the tail window

- The live body renders **only the last `LIVE_TAIL_ROWS = 4` wrapped rows**
  of the stream (summary or raw, same source preference as today), each on
  the `┊` rail as now.
- Above the tail, when anything is hidden, ONE honest elision row on the
  rail: `┊ … +N rows` (muted; `N` = wrapped rows currently hidden). It is
  a readout, not a button (ctrl+o is the affordance and is listed below).
- The bottom tail row ends with the **`▋` caret** (orange, the same glyph
  the composer/register vocabulary uses) at the exact character where the
  next quantum will land. If the escapement spec
  (docs/specs/escapement.md) is already landed, the caret advances in its
  word-steps; if not, on raw arrival — either way the caret exists after
  this change.
- Bound rationale: reasoning can run thousands of rows; a transcript
  surface must never be dominated by its most recessive block (§7.4 —
  "the most recessive thing in the pane"). Four rows + elision keeps the
  thought visible as a *ticker*, not a flood.

### 2.3 ctrl+o during streaming

- ctrl+o (via the existing `toggle_all_panels` path) while a live preview
  is up toggles the live block between the tail window and the **full live
  stream** (all rows, as today's behavior renders). The user's choice is
  remembered for the live phase only; the committed block starts from the
  standard settled fold state (user fold intent on the committed block
  keeps working via `set_panel_expanded_at` exactly as today).
- The live block participates in `toggle_all_panels` as a foldable header
  even before commit (it has a genuinely hidden body — the elided rows).
  When nothing is hidden (short thought, ≤ 4 rows), it does not participate
  (no no-op affordance — the house rule).

## 3 · What must NOT change (regression pins)

Every recon-verified settled behavior: fold rules incl. summary-only
non-foldable and redacted handling; the splice-above-committed-answer
anchoring (`stream_answer_start`, `shift_row_anchors`); telemetry patch on
`ProviderTurnCompleted` incl. the trim-alignment pin
(`trim_history_keeps_thinking_header_telemetry_index_aligned`); markdown
theme (dim+italic); finalize-never-reflows (preview rendered at final
content width). The existing tests at tui.rs:1309–1821 and 5056–5988 must
stay green unmodified (except where they assert the OLD live-preview shape —
update only those, and list each in the PR).

## 4 · Design-language amendments (same change)

§7.4: replace the "pulses" sentence with the lamp + caret as built; add the
tail-window rule (live body = last 4 rows + `… +N rows` elision; ctrl+o
opens the full stream) and the live elapsed readout. One sentence in §6: the
lamp is a state light, not a motion (no new motion enters the closed set).

## 5 · Acceptance criteria

1. Streaming: header shows `▾ THINKING ●` + live elapsed; commit drops the
   lamp and patches `↓tokens elapsed` exactly as before (existing test
   extended, not weakened).
2. A 40-row live stream renders 4 tail rows + `┊ … +36 rows`; a 3-row
   stream renders whole with no elision row; the tail row carries `▋` at
   the stream edge; caret position advances with arrival.
3. ctrl+o during streaming toggles tail window ⇄ full stream; a ≤4-row live
   block is not foldable; after commit, fold behavior is byte-identical to
   today for the same content.
4. Reduced motion: no behavioral difference (nothing here animates — assert
   the lamp/caret/window render identically; elapsed still updates, it is
   data).
5. Splice + telemetry pins all green (`reasoning_splices_above…`,
   `live_reasoning_previews_then_commits…`, telemetry patch + trim pins).
6. Redacted reasoning: never any live body rows beyond the placeholder,
   no caret, no elision row.
7. Golden frames: live tail window mid-stream (with lamp, elapsed, caret,
   elision row); the same block settled after commit.
8. `bash scripts/gate.sh` passes; §7.4/§6 amended.

## 6 · Out of scope

- The escapement itself (separate spec; integrate if landed).
- Any change to settled fold/splice/redaction logic.
- Tool-header tone unification (the recessive label divergence stays).
