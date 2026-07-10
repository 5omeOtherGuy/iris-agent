# SPEC — The job card: the pinned prompt, legible and honest

Status: approved for implementation · Surface: the pager sticky prompt
(`src/ui/tui/pager.rs` sticky_prompt_band, `src/ui/tui/screen.rs`
toggle_sticky_prompt, ctrl+o routing in `src/ui/tui_loop.rs`).
Complexity: LOW-MEDIUM. Iteration passes: none required beyond the
implementer's own checks (this is refinement of an existing band) — but the
ctrl+o re-route must be regression-tested carefully.

---

## 0 · Goal

The pinned user prompt is the machine's **job card** — the governing
instruction for everything on screen below it. Today it is the least
legible text in the pane (all-dim), hides its own size when collapsed, and
hijacks ctrl+o from fold toggling. Three fixes:

1. **Legibility**: the prompt text renders in body ink; the chrome around
   it stays muted.
2. **Honesty**: a collapsed multi-row prompt shows how much is hidden.
3. **Keys**: ctrl+o goes back to meaning "toggle folds", always; the band
   gets its own toggle.

## 1 · The band (after)

Collapsed:

```
▸ › Overhaul the settings menu. First, prune the settings and…   +4
```

- `▸ `, `› ` unchanged (muted bold). The prompt's first line: **body ink**
  (`panel_style`), no longer dim — the one piece of content in the top
  chrome, legible at a glance; the surrounding chrome tones make it read as
  chrome still. Truncation with `…` as today.
- Right-aligned dim `+N` when N wrapped rows are hidden (house `+N more`
  idiom, shortened — the band has no room for prose). Absent when nothing
  is hidden.

Expanded: unchanged structurally (all wrapped lines + closing rule), but
the text is ink and continuation rows keep their 4-space hang. The closing
rule stays muted.

## 2 · Keys and routing

- **ctrl+o**: remove the sticky-prompt pre-emption in tui_loop.rs
  (~2888–2890, 3305–3307). ctrl+o ALWAYS toggles transcript folds — its one
  meaning everywhere. (Recon confirmed the trap: with a sticky prompt
  showing, a user cannot ctrl+o their collapsed tool/thinking blocks.)
- **The band's own toggle**: mouse click on the band row (existing,
  unchanged), plus the key `o` while in pager mode (pager is a readout —
  list-state law makes single letters legal there). Before binding, grep
  the pager keymap for a conflict on `o`; if taken, choose the nearest free
  letter and document it. The pager help overlay (if one lists keys) gains
  the line; otherwise the `+N` hint is the affordance signal.
- Expansion state still resets to collapsed on each new user message.

## 3 · What must not change

Everything recon verified: pager-only gating (`view_rows >= 5`, `top > 0`,
yields to a search/selection row at the viewport top), band overwrites body
rows in place (never floats), binary-search anchor logic and its
trim/splice maintenance, click row targeting, one-row collapse for long
prompts. Existing pins pager.rs:1287–1421 and screen.rs:2941 stay green
(update only assertions on the text style and the ctrl+o route, listing
each in the PR).

## 4 · Design-language amendment (same change)

The band is currently undocumented in docs/TUI_DESIGN_LANGUAGE.md. Add a
short **§9.1.2 The job card** (or fold into the pager section §1.1 if it
reads better there — implementer's call, say which): pager-only; pinned
governing prompt; ink text in muted chrome; `+N` honesty; click/`o`
toggles; ctrl+o never routes here; yields to search matches.

## 5 · Acceptance criteria

1. Collapsed band: first line ink, chrome muted, dim right-aligned `+N`
   iff rows are hidden (0-hidden → no marker) — golden or span-level test.
2. Expanded band: ink text, muted rule; toggle via click and via the
   chosen key; state resets on new user message.
3. ctrl+o with a sticky prompt visible AND collapsed blocks in the
   transcript: the blocks toggle, the band does not (the regression test
   for the re-route).
4. The chosen band key does not collide with any existing pager binding
   (test or exhaustive keymap check in the PR).
5. Existing sticky-prompt pins green (with listed assertion updates only).
6. `bash scripts/gate.sh` passes; design doc gains the band's section.

## 6 · Out of scope

- Telemetry on the band (elapsed/turns live in the working indicator and
  session bar — one home per readout).
- An inline-mode sticky prompt.
- Multi-prompt history navigation from the band.
