# SPEC — Reactive density: tool previews breathe with terminal height

Status: implemented; message-width portion superseded · Surface: tool-output
previews (§8.1).
Complexity: MEDIUM-LOW. Iteration passes required after it works: 1.

---

## 0 · Goal

One print-time density adapts to terminal height instead of remaining fixed:

**Preview budgets breathe with height.** Tool-output previews are capped at a
fixed `MAX_TOOL_OUTPUT_ROWS = 8` regardless of whether the pane is 24 rows or
120. Make the cap viewport-aware at print time:
`preview_rows = clamp(pane_height / 5, 8, 24)`.

Testable outcome: on a 120-row pane a fresh tool block previews 24 rows; on a
24-row pane it previews 8 (the floor preserves behavior on small terminals).
Transcript messages independently use the available pane width; the earlier
96-column prose cap from this spec has been removed.

## 1 · Why

A fixed 8-row preview starves a 120-row terminal: the available height goes
unused. The fix is a **print-time decision** — rows are immutable once committed
to scrollback, and that is fine: the block honestly reflects the terminal it
was printed into. No reflow machinery or retroactive edits are required.

## 2 · Preview budgets

- Replace the fixed constant's *use* (not necessarily the constant — keep a
  named floor/ceiling) with a computed budget:
  `clamp(pane_height / 5, 8, 24)` rows, where `pane_height` is the last-known
  terminal height at the moment the block's rows are built.
- Divisor rationale (record it in a comment): a tool block must never
  dominate the pane — at height/5 the preview claims at most a fifth of the
  viewport, leaving the conversation the floor.
- The height must reach the row builders the same way width already does —
  thread the last-known `Size` (or its height) into the transcript append
  path; do NOT reach for a global.
- Applies to whatever `MAX_TOOL_OUTPUT_ROWS` governs today (EXPLORE/SHELL
  preview tails etc.). The elision line (`… +N more`, ctrl+o affordance) and
  the stored-handle behavior are unchanged — only the budget moves.
- Resize honesty: blocks printed before a resize keep their printed size;
  new blocks use the new height. State this in the §8.1 amendment (one
  sentence).

## 3 · Width policy amendment

The original implementation introduced a separate 96-column prose measure.
That limit is superseded: user, assistant, thinking, notice, and other
transcript messages use the available pane width. Wrapping remains semantic,
continuations retain their hanging indentation, and all rows remain bounded by
the pane.

## 4 · Design-language amendment

- §3 (Type): transcript content uses the available pane width while preserving
  semantic wrapping, rails, markers, and hanging indentation.
- §8.1: preview budget = `clamp(height/5, 8, 24)`, floor = the old fixed 8.

## 5 · Acceptance criteria

1. Preview budget: heights 20/24/40/60/120/200 → budgets 8/8/8/12/24/24
   (unit test on the clamp; one row-level test that a 30-line output at
   height 120 previews 24 rows + elision, and at height 24 previews 8 —
   today's exact output).
2. Existing narrow-width goldens are unchanged.
3. At 200 columns, user, assistant, thinking, and notice content extends beyond
   the former cap when content permits, while remaining within the pane.
4. Wrap continuations and hanging indents still align under their content.
5. Resize: a block printed at height 24 then a resize to 120 leaves the old
   block at 8 preview rows; the next block previews 24 (loop-level test if
   cheap, otherwise a unit test on the budget input + a code comment).
6. `bash scripts/gate.sh` passes; §3 and §8.1 amended.

## 6 · Out of scope

- Retroactive reflow of printed blocks on resize.
- Any horizontal centering or two-column layouts.
- Changing `MAX_TOOL_OUTPUT_LINE_CHARS`, `MAX_EXEC_STREAM_BYTES`, or the
  pager's own windowing.
