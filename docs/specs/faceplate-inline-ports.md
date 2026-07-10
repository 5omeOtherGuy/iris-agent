# SPEC — Faceplate inline ports: hatches, not doors

Status: approved for implementation · Target branch: `worktree-tui-showcase`
(worktree `.claude/worktrees/tui-showcase`), on top of `ceea91a`.
Supersedes the "settings is home" modal-return mechanism landed in `fa93453`.

---

## 0 · Goal (the one-sentence contract)

Pressing `↵` on any `▸` port row of the settings faceplate must **expand that
surface in place** — the marker flips to `▾` and the surface's rows print
indented directly beneath the port row, inside the same panel — instead of
replacing the faceplate with a different modal. `esc` (or `↵` on the header)
folds it back. At no point during expand, collapse, or any port operation does
the screen show a frame without the faceplate.

**Definition of done, testable:** after this change, opening `/settings`,
navigating to each of the four ports (`model`, `model scope`, `providers`,
`permissions`), pressing `↵`, operating every function the old sub-modal
offered, and pressing `esc` — all without a single modal swap — passes the
acceptance criteria in §7, `scripts/gate.sh` passes clean (fmt + clippy -D
warnings + full test suite), and the standalone menu modals listed in §4.2 no
longer exist in the codebase.

---

## 1 · Background — what exists today (read these first)

Read before writing code:

- `docs/TUI_DESIGN_LANGUAGE.md` — §5 (symbol vocabulary), §6 (motion), §10
  (overlays), **§10.1 (the faceplate — this spec amends it)**.
- `src/ui/settings_menu.rs` — `SettingsPanel`, `RowId`, `SECTIONS`,
  `DisplayRow`/`display_rows()`, `Archetype`, `switch_spans`, `LABEL_W`,
  `render_budgeted`, the footer, the flash.
- `src/ui/modal.rs` — `Modal`, `ModalAction`, `ModalOutcome`, and the four
  sub-surfaces being absorbed: `ModelPicker`, `ScopedModels`, `TrustMenu`,
  `MethodSelect`/`ProviderSelect` (plus the dialogs that stay: `LoginDialog`,
  `ApiKeyDialog`, `SwitchContextPrompt`).
- `src/ui/picker.rs` — `settings_snapshot`, `open_settings`/`open_settings_at`,
  `apply_action`, `ActionResult`.
- `src/ui/tui_loop.rs` — `run_modal_phase` (the `settings_return` /
  `settings_return_row` plumbing this spec deletes), slash dispatch around
  lines 1394–1620.
- `src/ui/tui/screen.rs` — `menu_room`, the modal render path
  (`render_budgeted(menu_inner_width, budget)`).
- Golden tests: `src/ui/tui.rs` (`faceplate_snapshot()` fixture and the three
  faceplate frame goldens; picker goldens that will be deleted/replaced).

Current behavior being replaced: `↵` on a port emits
`OpenModelPicker` / `OpenScopedModels` / `OpenLoginMethod` / `OpenTrustMenu`;
`apply_action` swaps `Modal::Settings` for the sub-modal; when it closes,
`run_modal_phase` re-opens a **rebuilt** panel at the launching row
(`settings_return`). It works, but it is a door: the faceplate vanishes and
returns. The instrument idiom is a **hatch**: the panel never leaves; the row
opens downward and the deeper surface is revealed on the same silkscreen.

Vocabulary note: `symbols.rs` already owns the disclosure pair —
`COLLAPSED` (`▸`) and `EXPANDED` (`▾`). Reuse those constants for the port
marker. **No new glyph is introduced by this spec.**

---

## 2 · The framework — hatch mechanics (applies to all four ports)

### 2.1 State

`SettingsPanel` gains:

```rust
/// The one open hatch, if any. Accordion: opening a port collapses any other.
expanded: Option<RowId>,
```

Exactly **one** hatch may be open at a time (a real instrument does not have
every access panel hanging open). Expanding port B while port A is open
collapses A in the same keypress.

### 2.2 Navigation model

The selectable-row list (`controls`) becomes a **flattened** list rebuilt
whenever `expanded` changes: top-level control rows in `SECTIONS` order, with
the expanded port's **child rows spliced in immediately after the port
header**. Introduce a two-level row identity, e.g.:

```rust
enum PanelRow {
    Top(RowId),
    Child(ChildId),   // one variant per port surface; carries the item key
}
```

- `↑`/`↓` walk the flattened list exactly as today (wrapping; section
  headers, blanks, and read-only silkscreen lines remain unselectable).
- Cursor and flash must key on **row identity**, not on positional index —
  expanding a port above the cursor must not silently move the selection, and
  a flash armed on a row must survive the list reflowing. (Today `flash` is
  `(usize, u8)`; fix this while restructuring.)
- After a collapse, the cursor lands on the port header row.

### 2.3 Keys

| Context | Key | Effect |
|---|---|---|
| cursor on collapsed port header | `↵` | expand (accordion), arm the 2-tick flash on the header |
| cursor on expanded port header | `↵` | collapse, flash header |
| cursor anywhere, hatch open | `esc` | collapse the hatch (cursor → header); panel stays open |
| cursor anywhere, no hatch open | `esc` / `ctrl+c` | close the panel (unchanged) |
| cursor on child row | per-port verbs (§3) | — |

`←`/`→` are **never** collapse verbs — they stay reserved for detents/effort
(keymap honesty; no accidental collapse). Two verbs fold a hatch: `↵` on the
header, `esc` anywhere. `esc` with an active type-to-filter (§3.2) clears the
filter first; the second `esc` collapses.

### 2.4 Rendering

- The port header keeps its label + summary value line; only the marker
  changes: `▸` collapsed → `▾` expanded. The summary stays printed (and stays
  honest) while open — e.g. `model scope       ▾ 6 of 13 enabled`.
- Child rows print at **4-space indent** (top-level rows are at 2). Child
  rows that are themselves label+control pairs (the permissions switches)
  align their control column to the house grid: label field width
  `LABEL_W − 2` so their tracks line up with the panel's control column.
  List-like children (models, providers) are `marker + name … muted right
  column`, the §10 Picker row grammar, on the panel measure.
- The selected child row highlights exactly like a top-level row (surface
  fill across the measure, bold label — never a border).
- **Footer is contextual per row** (existing keymap-honesty rule): the
  selected row prints its own true verbs (§3 tables). The panel-level footer
  (`↑↓ select · … · esc close`) applies only when no hatch is open.
- **Windowing:** the expansion adds rows to the same `render_budgeted`
  window. One window for the whole faceplate — an expansion never gets an
  inner scrollbar. The cursor (including child rows) must always be visible;
  the `(n/N)` position row counts the flattened selectable list; the masthead
  stays pinned. No inner row cap: a 13-model scope list windows through the
  panel like any other overflow.
- **Motion:** expansion/collapse is instant reveal (print, no animation).
  The header's 2-tick detent flash is the acknowledgment, gated by reduced
  motion as everywhere else (§6).
- **Monochrome/narrow:** `▸`→`▾` is a shape change (passes the monochrome
  test by construction). At narrow widths child rows degrade like their
  archetypes (switch → rotary form; list rows drop the muted right column
  before truncating the name; footers drop whole fields — reuse the existing
  degradation helpers, do not invent new rules).

### 2.5 Dialog-guards and the stash (what still overlays)

Genuine **dialogs** — interrupts with their own lifecycle, not menus — are
still allowed to overlay the faceplate:

- `SwitchContextPrompt` (the large-context advisory on a model switch),
- `LoginDialog` (OAuth device-code/browser flow),
- `ApiKeyDialog` (secret entry).

For these, replace the deleted `settings_return` row-rebuild with a
**stash-and-restore**: `run_modal_phase` keeps the outgoing
`Box<SettingsPanel>` aside (e.g. `stashed_panel: Option<Box<SettingsPanel>>`)
when a guard replaces `Modal::Settings`; when the guard resolves (confirm,
cancel, success, failure — every path), the loop **refreshes the panel's
snapshot** (provider badges, catalog, current model — a login can authenticate
new models) while **preserving cursor + expansion**, and re-opens it **before
the next draw** (this is the invariant that killed the jank in `fa93453`;
keep the same reopen-before-draw discipline). The stash is cleared whenever
the panel is in front.

---

## 3 · Per-port content (what each hatch reveals)

### 3.1 `model` — the unified Model & reasoning picker, inline

The rotary–port hybrid keeps both verbs on the header: `←`/`→` cycles scoped
models (unchanged `CycleModel`), `↵` now expands instead of opening the
picker modal.

```
ENGINE
  model             ▾ gpt-5.5 ┊ openai-codex
    ◉ gpt-5.5                          openai-codex   default
    ○ claude-sonnet-5                  anthropic
    ○ claude-opus-4-8                  anthropic
    ○ gemini-3-pro                     google
  reasoning         ○ off  ○ minimal  ○ low  ◉ medium  ○ high  ○ xhigh
```

- One child row per authenticated model, persisted-default first then by
  provider (the existing `order_by_default` ordering). `◉` marks the **active
  session model** (orange — selection color); the persisted default carries
  the quiet `default` tag; provider is the muted right column.
- **The panel's own `reasoning` row IS the picker's effort track.** While the
  hatch is open, the reasoning row (sitting directly below the expansion)
  **live-tracks the highlighted candidate**: arrowing over models re-renders
  it with that model's supported levels and the target effort clamped to
  them (the existing `display_effort` clamp semantics — navigation never
  mutates the target). Collapse without selecting reverts it to the active
  model's truth. This is the §10 "adjacent things share one picker" rule made
  literal — there is no second, duplicated track inside the expansion.
- Child-row keys (footer: `←→ reasoning · ↵ set default · s session · esc collapse`):
  - `←`/`→` — click the effort detent for the highlighted candidate
    (existing `cycle_effort` clamp-at-stops behavior; the reasoning row below
    flashes its detent).
  - `↵` — emit `SelectModel { save_default: true }` with the displayed
    (clamped) effort. On success the header value line updates, the header
    flashes, and the hatch **stays open** (an instrument hatch does not slam
    itself; the operator closes it).
  - `s` — `SelectModel { save_default: false }` (session only), same feedback.
- If the switch triggers the large-context advisory, the
  `SwitchContextPrompt` overlays as a dialog-guard (§2.5) and the faceplate
  returns expanded, cursor intact, whichever way it resolves.

### 3.2 `model scope` — the checklist, inline

```
  model scope       ▾ 6 of 13 enabled · unsaved
    ◉ openai-codex/gpt-5.5
    ◉ anthropic/claude-sonnet-5
    ○ anthropic/claude-haiku-4-5
    ○ google/gemini-3-flash
```

- One child row per authenticated candidate; enabled first in configured
  order, then the rest in registry order (existing `ScopedModels::rebuild`
  ordering). `◉`/`○` per row; `enabled = None` ("all enabled") renders as
  today's no-checkmark-column form.
- Keys carry over 1:1 from the modal, same emits, footer-honest:
  `↵ toggle · ctrl+a all · ctrl+x none · ctrl+p provider · alt+↑↓ reorder ·
  ctrl+s save · esc collapse` (at narrow widths the footer drops whole
  fields, rightmost first — existing rule).
  - `↵` toggle → `ApplyScoped` (live, session) — unchanged semantics.
  - `ctrl+s` → `SaveScoped` (persist) — unchanged.
- **Persistence semantics are deliberately NOT unified** with the faceplate's
  save-on-click rule: scope is a live session filter with explicit persist
  (pi-mono parity). The header line stays honest instead: it prints
  `n of N enabled` (or `all enabled`) **plus `· unsaved`** while the live
  scope differs from the persisted one; the tag clears on `ctrl+s`.
- **Type-to-filter** carries over: printable characters filter the child rows
  while the cursor is inside this hatch (they are not panel-level keys).
  `backspace` edits the filter; `esc` clears an active filter first,
  collapses second; `ctrl+a`/`ctrl+x` operate on the filtered set as today.

### 3.3 `providers` — flat provider list with credential badges

```
  providers         ▾ 3 connected
    ◉ anthropic            subscription
    ◉ openai-codex         api key
    ◉ openrouter           api key
    ○ google               —
```

- One child row per known provider (registry order): `◉` credentialed /
  `○` not; the muted right column is the credential badge (`subscription`,
  `api key`, `—`). Never a secret.
- Keys (footer: `↵ login · a api key · x logout · esc collapse`, with `x`
  printed only on credentialed rows, `↵ login` only where a method exists):
  - `↵` — the provider's **primary** method: `BeginLogin(provider)` when it
    supports subscription/OAuth, else `OpenApiKeyDialog(id)`. The resulting
    `LoginDialog`/`ApiKeyDialog` overlays as a dialog-guard (§2.5); on any
    resolution the faceplate returns, hatch open, cursor on the same
    provider, badge and header count refreshed — and the ENGINE rows refresh
    too (a new login can grow the model catalog).
  - `a` — force the API-key path: `OpenApiKeyDialog(id)`.
  - `x` — `Logout(id)` on a credentialed row (immediate, as today's
    `/logout`; the row's `◉` drops to `○`, badge to `—`, header count
    decrements, row flashes).
- This absorbs the method-first `/login` flow: **provider-first, then
  method** is the new shape. `MethodSelect` and both `ProviderSelect`
  purposes (Login/Logout) are deleted (§4.2). `ProviderPurpose::ApiKeyLogin`
  survives only if the `ApiKeyDialog` plumbing needs it; otherwise remove.

### 3.4 `permissions` — the trust surface as real controls

```
  permissions       ▾ per-tool + bash grants
    write           ○ ask  ◉ always
    edit            ◉ ask  ○ always
    bash: cargo test                    ↵ revoke
    bash prefix: git                    ↵ revoke
    sandbox         read-only ┊ workspace-write
```

- The per-tool grants (`write`, `edit` — `POLICY_TOOLS`) become **two-detent
  switches** rendered through `switch_spans` on the child grid: vocabulary
  `ask · always`. `←`/`→` clicks the detent and emits
  `EditPolicy(GrantTool/RevokeTool)` immediately (position IS state — this
  replaces the old `↵ toggle` + detail-text idiom, which was a menu
  convention, not an instrument one). Standard 2-tick flash.
- Stored bash grants (exact, then prefix) are **revoke-only rows**: `↵` emits
  `EditPolicy(RevokeBashExact/RevokeBashPrefix)`; on success the row
  disappears from the flattened list (cursor moves to the next row) — no
  confirm dialog, matching today.
- The sandbox posture stays a **read-only dim silkscreen line** (not
  selectable — it joins headers/blanks in the skip set).
- Footer: switch rows `←→ set · esc collapse`; bash rows `↵ revoke · esc
  collapse`.

---

## 4 · Entry-point unification & deletions

### 4.1 Slash commands (and keys) route to the faceplate

Add a constructor `SettingsPanel::with_expanded(snapshot, port, /*cursor:*/ …)`
(and `picker::open_settings_expanded(harness, switch, port)`) that opens the
panel with the given hatch open and the cursor placed on the most useful
child row. Rewire:

| Entry | Opens |
|---|---|
| `/settings`, `ctrl+,`, deferred-settings path | faceplate, no hatch (unchanged) |
| `/model` (bare), bare `/reasoning`, `IdleKey::OpenModelPicker` (ctrl+p picker key) | faceplate, `model` hatch open, cursor on the **active** model row |
| `/scoped-models` | faceplate, `model scope` hatch open, cursor on first child |
| `/trust`, `/permissions` (bare) | faceplate, `permissions` hatch open, cursor on first child |
| `/login` (bare) | faceplate, `providers` hatch open, cursor on first **un**credentialed row (else first) |
| `/logout` (bare) | faceplate, `providers` hatch open, cursor on first **credentialed** row (else first) |

Typed fast paths are untouched: `/model <id>`, `/reasoning <level>` keep
resolving directly with no UI. Update the `/model`, `/reasoning`,
`/scoped-models`, `/trust`, `/permissions`, `/login`, `/logout` descriptions
in `src/ui/slash.rs` to name the faceplate (e.g. `/login` → "Connect a
provider (opens settings › providers)").

Note `←`/`→` model **cycling** from the composer (Ctrl+P quick-cycle, if
bound) is not affected — only the *picker-opening* paths move.

### 4.2 Deletions (this spec removes code; guard with grep)

- `Modal::Model(ModelPicker)`, `Modal::Scoped(ScopedModels)`,
  `Modal::Trust(TrustMenu)`, `Modal::LoginMethod(MethodSelect)`,
  `Modal::Providers(ProviderSelect)` — variants, structs, renders, keymaps,
  and their `overlay_menu` titles (`MODEL & REASONING`, `Scoped models`,
  `Project permissions`, `Login`, `Select provider`, `Logout`).
- `ModalAction::{OpenModelPicker, OpenScopedModels, OpenLoginMethod,
  OpenTrustMenu, ChooseLoginMethod, BackToLoginMethod}` and
  `LoginMethod`/`MethodSelect` types if now unreferenced.
- `settings_return` / `settings_return_row` in `tui_loop.rs` (replaced by the
  §2.5 stash).
- `open_model`/`open_scoped`/`open_trust`/`open_login` become either deleted
  or refactored into snapshot-builders feeding §5 (keep whatever assembles
  rows; delete whatever builds modals).
- Their unit/golden tests migrate to faceplate-expansion tests (§8), they are
  not just deleted: every behavior they pinned (ordering, clamping, badge
  text, effort-never-truncates) must be re-pinned on the inline form.

`ModelPicker`'s *logic* (ordering, `display_effort` clamping, select-emit)
moves into the model hatch; do not fork it — move it.

### 4.3 What explicitly stays

- `SwitchContextPrompt`, `LoginDialog`, `ApiKeyDialog` (dialog-guards, §2.5).
- `SessionPicker`, `Tasks` — different surfaces, out of scope.
- All `config::save_*` semantics, `ApplyScoped` vs `SaveScoped` two-tier
  persistence, `EditPolicy` plumbing, `SelectModel` resolution: **no
  persistence behavior changes anywhere in this spec.** This is a surface
  refactor; the actions and their effects are frozen.

---

## 5 · Data & plumbing

`Snapshot` (in `settings_menu.rs`, built by `picker::settings_snapshot`)
grows the hatch payloads — presentation-ready, disk-free, no secrets:

```rust
/// ENGINE › model hatch: authenticated catalog, default-first.
catalog: Vec<ModelChoice>,        // { qualified, display, provider_label,
                                  //   levels: Vec<(ReasoningEffort, &'static str)>,
                                  //   is_current, is_default }
/// ENGINE › scope hatch.
scope_candidates: Vec<ScopeChoice>, // registry order; { qualified, provider_label }
scope_enabled: Option<Vec<String>>, // None = all (existing collapse_full rule)
/// ENGINE › providers hatch.
providers: Vec<ProviderStatus>,   // { id, name, badge, oauth_capable, credentialed }
/// SAFETY › permissions hatch.
policy: PolicySnapshot,           // { granted_tools, bash_exact, bash_prefix,
                                  //   sandbox: Option<String> }
```

- `Modal::Settings` is already boxed; the larger snapshot rides along.
- Snapshot refresh points: after every dialog-guard resolution (§2.5), after
  `SelectModel`/`CycleModel` success, after `Logout`, after `EditPolicy` —
  the loop refreshes the affected snapshot fields (or rebuilds the snapshot
  wholesale) while preserving panel cursor/expansion/filter state. The panel
  itself stays the display truth between refreshes exactly as today (it
  already clicked the detent; `ActionResult::Keep` on success).
- `apply_action` in `picker.rs` handles the child-row emits with the existing
  arms (`SelectModel`, `ApplyScoped`, `SaveScoped`, `EditPolicy`,
  `BeginLogin`, `OpenApiKeyDialog`, `Logout`) — mostly re-pointing "which
  modal comes back" from sub-modal to panel-with-state.

---

## 6 · Design-language obligations

Update `docs/TUI_DESIGN_LANGUAGE.md` in the same change:

- **§10.1 port archetype** — rewrite: a port is a **hatch**: `▸` expands in
  place to `▾` + indented child rows; one hatch open at a time; `esc`/`↵`
  folds; no surface replacement; the model row hybrid keeps `←→ cycle ·
  ↵ open`. Delete the "settings is home / re-opens on the port row" language
  (nothing closes anymore); keep the dialog-guard exception and the
  reopen-before-draw invariant for guards.
- **§10 Picker paragraph** — the model switcher bullet now describes the
  hatch (`/model` and bare `/reasoning` open the faceplate's ENGINE hatch);
  the "adjacent things share one picker" law is unchanged and now enforced
  structurally (the reasoning row IS the track).
- §5 needs no new symbols (reuses `EXPANDED`/`COLLAPSED`).
- The faceplate mock in §10.1 gains one expanded-hatch illustration.

House rules that apply to this work (non-negotiable):

- **≥3 iteration passes** after "it works": one for mechanics/jank, one for
  honesty (every printed value true, every footer verb real), one adversarial
  (narrow widths, short heights, empty states: zero providers, zero bash
  grants, one model, `enabled = Some([])`, off-vocabulary config values).
- `scripts/gate.sh` with `set -o pipefail` if piped (a bare pipe masks its
  exit code — this has bitten before).
- Golden tests first, then `scripts/tui-live.sh` to confirm by eye
  (tmux; try `--size 200x50`, `100x40`, `80x24`, and a 20-row height).
- **`~/.iris/settings.json` is multi-writer** — the user's own iris instance
  runs concurrently (check `pgrep -a iris` / `tmux list-panes -a`) and
  persists his choices. Never blind-restore a backup over it; never attribute
  its changes to your own testing without checking. `cfg(test)` already
  redirects test config I/O to a scratch path (`config::global_path`) — keep
  that property intact.

---

## 7 · Acceptance criteria (each one a test or a documented manual check)

Framework:

1. `↵` on each collapsed port expands it: marker `▾`, children present in the
   flattened selectable list, cursor still on the header, header flash armed
   (and not armed under reduced motion).
2. `↵` on the expanded header collapses; `esc` from any child row collapses
   (cursor → header); `esc` with no hatch open closes the panel; the old
   two-step is preserved: hatch open + `esc` `esc` = collapse, then close.
3. Accordion: expanding `providers` while `model scope` is open collapses
   scope in the same keypress; at most one `▾` ever renders.
4. `↑`/`↓` traverse header → children → next control, wrapping across the
   whole flattened list; section headers, blanks, and the sandbox line are
   never selectable.
5. Expanding a port above the cursor does not move the selection to a
   different row (identity-keyed cursor); a flash survives a reflow.
6. Windowing: with the model hatch open at a height where the panel must
   window, the cursor is always within the visible window, `(n/N)` counts
   flattened selectable rows, and the masthead row is pinned.
7. **No-jank invariant:** across expand, collapse, every child-row emit, and
   every dialog-guard round trip, no draw occurs with `screen.modal == None`
   (assert at the loop level, same style as the `fa93453` reopen-before-draw
   test).

Model hatch:

8. Rows ordered default-first; `◉` on the active model; `default` tag on the
   persisted default; provider column muted.
9. Arrowing over candidates re-renders the panel's `reasoning` row with that
   candidate's levels, target clamped, **without mutating the target**
   (arrow over a low-cap model and back: target intact — the existing
   `display_effort` pin, re-asserted inline).
10. `←`/`→` on a candidate clamps at that model's level stops; `↵` emits
    `SelectModel{save_default: true}` with the displayed effort; `s` emits
    `save_default: false`; after either, the header value line shows the new
    model, the hatch remains open, and the reasoning row shows the committed
    truth.
11. Collapse without selecting reverts the reasoning row to the active
    model's real track.
12. A selection that trips the large-context advisory overlays
    `SwitchContextPrompt`; on **both** confirm and cancel the faceplate
    returns with hatch open and cursor on the same candidate.

Scope hatch:

13. `↵` toggles a row and emits `ApplyScoped`; header prints `n of N enabled`
    live; `all enabled` when `None`; `· unsaved` appears while live scope ≠
    persisted scope and clears on `ctrl+s` (`SaveScoped`).
14. `ctrl+a`/`ctrl+x`/`ctrl+p`/`alt+↑↓` behave exactly as the old modal
    (including filtered-set scoping); the full-coverage list collapses to
    `None` (`collapse_full` re-pinned); explicit `Some([])` renders and
    survives as "nothing enabled".
15. Type-to-filter narrows child rows; `esc` clears the filter first,
    collapses second; filter state does not leak to other hatches or persist
    across collapse.

Providers hatch:

16. Rows show `◉`/`○` + badge; footer only advertises verbs that exist for
    the selected row (`x` only when credentialed, `↵ login` only when a
    method exists).
17. `↵` on an OAuth-capable provider emits `BeginLogin`; `a` emits
    `OpenApiKeyDialog`; after the dialog resolves (success/cancel), the
    faceplate returns expanded, same cursor, badge + `n connected` header +
    ENGINE catalog refreshed.
18. `x` on a credentialed provider emits `Logout`, the row drops to `○ · —`,
    the header count decrements, and the catalog/scope rows refresh.

Permissions hatch:

19. `write`/`edit` render as `ask · always` two-detent switches on the child
    grid (tracks aligned to the control column); `←`/`→` emits the matching
    `EditPolicy` grant/revoke immediately and clamps at stops.
20. Bash grant rows revoke on `↵` and vanish from the list; the empty state
    (no grants) prints the quiet empty row, not nothing; the sandbox line is
    dim, read-only, unselectable.

Entry points & deletions:

21. `/model`, bare `/reasoning`, and the ctrl+p picker key open the faceplate
    with the model hatch open, cursor on the active model; `/scoped-models`,
    `/trust`, `/permissions`, `/login`, `/logout` land per the §4.1 table;
    `/model <id>` and `/reasoning <level>` still resolve with no UI.
22. The five deleted modal variants, the four `Open*` actions, and
    `settings_return_row` no longer exist (compile-level; plus a grep in the
    PR description). No test references the old overlay titles.
23. `scripts/gate.sh` passes (fmt, clippy -D warnings, full suite, locked).

Live (manual, tmux, documented in the PR):

24. At 200×50, 100×40, 80×24, and ~20 rows: expand each hatch, operate one
    control in each, collapse — no flicker frame, no masthead loss, no
    footer lies, composer never eaten.

---

## 8 · Test plan (where the criteria live)

- **Unit tests** in `settings_menu.rs`: framework criteria 1–5, hatch
  content criteria 8–11, 13–16, 19–20 (panel-level, emit-assertion style —
  the existing tests are the template).
- **Loop tests** in `tui_loop.rs`: criteria 7, 12, 17–18, 21 (the
  stash/restore, reopen-before-draw, slash routing).
- **Golden frames** in `src/ui/tui.rs`: extend `faceplate_snapshot()`;
  goldens for (a) model hatch open on a tall pane, (b) scope hatch windowed
  on a short pane, (c) permissions hatch with a bash grant, replacing the
  deleted picker goldens.
- **Migrated pins:** every behavioral assertion in the deleted modal tests
  (ordering, clamping, badges, `collapse_full`, effort preservation) must
  reappear against the inline surface — list the mapping in the PR.

---

## 9 · Out of scope (do not do these here)

- Any persistence-semantics change (scope two-tier apply/save stays; skip-
  approvals stays session-only; no new settings keys).
- Animating the expansion (sliding rows). Print instantly; flash is the
  acknowledgment.
- Nested hatches (a hatch within a hatch). The providers method choice was
  deliberately flattened to `↵`/`a`/`x` instead.
- Touching `SessionPicker`/`Tasks`/resume surfaces, the slash palette, or
  the start page.
- The stale skill copy `.claude/skills/iris-tui/DESIGN-LANGUAGE.md` (lives in
  the main checkout; separate task).
