# SPEC — Reactive density: the pane breathes with its terminal

Status: approved for implementation · Surface: tool-output previews (§8.1),
prose wrapping (§3/§4).
Complexity: MEDIUM-LOW. Iteration passes required after it works: 1.

---

## 0 · Goal

Two print-time densities that adapt to the terminal instead of being fixed:

1. **Preview budgets breathe with height.** Tool-output previews are capped
   at a fixed `MAX_TOOL_OUTPUT_ROWS = 8` (src/ui/tui.rs:115) regardless of
   whether the pane is 24 rows or 120. Make the cap viewport-aware at print
   time: `preview_rows = clamp(pane_height / 5, 8, 24)`.
2. **Prose gets a measure.** Assistant prose, thinking bodies, and notices
   currently wrap at the full pane width; at 200 columns that is an
   unreadable line length. Cap the *prose* measure at
   `min(content_width, 96)` columns while mechanical output (tool output,
   diffs, code blocks, rules/dividers, session chrome) keeps the full pane.

Testable outcome: on a 120-row pane a fresh tool block previews 24 rows; on a
24-row pane it previews 8 (exactly today's behavior — the floor IS the status
quo, so nothing regresses on small terminals). On a 200-column pane,
assistant paragraphs wrap at 96 columns while a diff and a `$ command` block
use the full width, and every existing golden at ≤ 96 columns is
byte-identical.

## 1 · Why

Density is a function of the vessel. A fixed 8-row preview starves a 120-row
terminal (you paid for the height; the instrument ignores it), while
full-width prose on an ultrawide punishes the reader (the eye loses the line
on the way back). Both fixes are **print-time decisions** — this codebase's
rows are immutable once committed to scrollback, and that is fine: the block
honestly reflects the terminal it was printed into. No reflow machinery, no
retroactive edits.

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

## 3 · The measure

- New constant `PROSE_MEASURE: usize = 96` (columns). In the wrap layer
  (src/ui/tui/wrap.rs), prose-classed rows wrap to
  `min(content_width, PROSE_MEASURE)`; mechanical rows keep `content_width`.
- Prose-classed: assistant message bodies (markdown paragraphs, list items,
  headings), thinking bodies, notices, plan-step notes, user message bodies.
- Mechanical (full width): fenced/indented code inside assistant markdown,
  tool block bodies (EXPLORE/SHELL/EDIT output), diffs, approval bodies,
  turn dividers and every rule, the session bar, composer, overlays/menus.
- The measure applies to TEXT WRAP ONLY — markers, rails, and indents render
  exactly as today; nothing is centered; the right side simply rags at 96.
- Tables in markdown (if rendered) count as mechanical.

## 4 · Design-language amendments (same change)

- §3 (Type) or §4 (Spacing): a short "Measure" clause — prose wraps at
  `min(pane, 96)`; mechanical output uses the pane; printed blocks reflect
  the terminal they were printed into (no retroactive reflow).
- §8.1: preview budget = `clamp(height/5, 8, 24)`, floor = the old fixed 8.

## 5 · Acceptance criteria

1. Preview budget: heights 20/24/40/60/120/200 → budgets 8/8/8/12/24/24
   (unit test on the clamp; one row-level test that a 30-line output at
   height 120 previews 24 rows + elision, and at height 24 previews 8 —
   today's exact output).
2. Existing goldens: every golden rendered at ≤ 96 columns is unchanged
   (this pins the no-regression floor).
3. Measure: at 200 columns, an assistant paragraph's longest rendered line
   ≤ 96 + indent; a code fence in the same message renders wider than 96
   when its content is wider; a diff and a SHELL body use full width; a
   notice wraps at the measure.
4. Wrap continuations and hanging indents still align under their content
   at the measure (no marker drift).
5. Resize: a block printed at height 24 then a resize to 120 leaves the old
   block at 8 preview rows; the next block previews 24 (loop-level test if
   cheap, otherwise a unit test on the budget input + a code comment).
6. `bash scripts/gate.sh` passes; §3/§4 and §8.1 amended.

## 6 · Out of scope

- Retroactive reflow of printed blocks on resize.
- Any horizontal centering or two-column layouts.
- Changing `MAX_TOOL_OUTPUT_LINE_CHARS`, `MAX_EXEC_STREAM_BYTES`, or the
  pager's own windowing.
