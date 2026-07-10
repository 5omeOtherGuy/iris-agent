# SPEC — The console drawers: tree & git console refinement

Status: approved for implementation · Surface: the session-bar dropdowns
(§9.1.1), `src/ui/tui/session_menu/{mod,tree_menu,git_menu}.rs`.
Complexity: MEDIUM-LOW. Iteration passes required after it works: 1.

---

## 0 · Goal

Bring the two drawers up to the faceplate bar. They are already deeply
built (group silkscreens, dotted leaders, honest footers, confirm gating,
the `▋` filter caret) — this is **conformance + density refinement**, not a
redesign. Four fixes and one signature density upgrade.

Testable outcome: no hardcoded colors remain; tree attribution survives
re-rooting; collapsed directories report the git state inside them; the
per-frame deep clone is gone; every changed row passes the existing width
and readonly pins.

## 1 · The five changes

### 1.1 Theme conformance (bug): tree file names

`tree_menu.rs` (~line 648) paints file names with a hardcoded
`Color::Gray`, bypassing the palette roles every other surface uses. Use
the themed role (`stdout()` — the "lighter than muted" content grey — or
`muted()`, whichever the surrounding rows use; pick by comparing against
the drawer's other content text and say which in the PR). A theme rotary
click on the faceplate must re-skin these rows like everything else.

### 1.2 Attribution must survive re-rooting (bug)

Today the `◉ open` / `◇ iris` / `± yours` right-column markers silently
vanish when the tree is re-rooted above/below the cwd (`root != cwd`
guard). The markers describe FILES, not the viewpoint — compute the
comparison paths correctly for any root (join/relativize against the root)
so attribution renders wherever the file appears. If a genuine ambiguity
exists for roots outside the repo, degrade per-row (no marker for
out-of-repo rows), never all-or-nothing.

### 1.3 The signature: dirty rollups on collapsed directories

A collapsed directory currently shows only `N files`. Replace with a
**state rollup** so the tree GUIDES the eye to where change lives:

```
 ▸ src/                                   ±3 ◇1 · 41 files
 ▸ docs/                                        12 files
```

- `±N` (orange) = user-dirty files anywhere beneath; `◇N` (muted) =
  iris-ledger files beneath; either omitted at zero; file count keeps its
  muted tail position (drop the count first if the row runs out of width —
  state outranks inventory).
- Same vocabulary as the session bar's state cluster (§9.1) — no new
  glyphs.
- Expanded directories don't carry rollups (their children speak for
  themselves).
- Costs: compute rollups from the same `status.user_paths`/`iris_paths`
  sets already fetched — prefix-match against the dir path; no new git
  calls.

### 1.4 Perf hygiene: stop cloning the tree every frame

`render_lines` clones the whole `TreeMenu` (children/expanded maps) per
frame to get `&mut` for the listing cache. Restructure so rendering
borrows (interior mutability for the cache, or pre-compute the listing on
state change) — mechanical fix, behavior identical, existing tests prove
it.

### 1.5 Small honesty polish (do all)

- Git console `SWITCH` overflow row already says `… N more · / to filter`
  — mirror the same affordance hint on the tree's `… N more` cap row
  (`… N more · / to filter`).
- Confirm-mode footers: verify every key named is live and every live key
  is named (recon found none missing — re-verify after changes; it is a
  house invariant).
- Readonly mode must dim the NEW rollup markers like everything else.

## 2 · Design-language amendments (same change)

§9.1.1: one sentence — collapsed tree directories carry the §9.1 state
cluster as a rollup (`±N ◇M`), count drops before state at width. Nothing
else changes doctrinally.

## 3 · Acceptance criteria

1. Zero `Color::` literals in session_menu/ (grep-level; palette roles
   only).
2. Re-root the tree up one level: a dirty file still shows `± yours`; an
   `@`-referenced file still shows `◉ open` (new tests).
3. Rollups: a collapsed dir with 2 user-dirty + 1 iris file renders
   `±2 ◇1 · N files`; zero-state dirs render count only; expanded dirs
   render no rollup; narrow width drops the count before the state; rollup
   is dim under readonly (each a test).
4. No `TreeMenu` clone in the render path (assert by code structure; the
   existing render tests stay green).
5. Tree cap row carries the filter hint.
6. Existing width-safety, readonly, and interaction pins in
   tree_menu.rs:714–855 and git_menu.rs:1214–1704 all green.
7. `bash scripts/gate.sh` passes; §9.1.1 amended.

## 4 · Out of scope

- Any git-console structural change (its groups/confirms/creates are
  right). No per-branch ahead/behind (needs git calls we don't make).
- The jj console (read-only by backend maturity, not by neglect).
- New keybindings, new modes, new actions.
