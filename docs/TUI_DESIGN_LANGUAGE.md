# Iris TUI — Design Language (canonical)

> **This document is ground truth.** It is the exhaustive specification of the
> Iris terminal-agent interface: every surface, every block, every symbol, every
> spacing rule, and the invariants a build must not violate. Where any other
> file in this system disagrees with this one, **this one wins**. `readme.md` is
> the guide and index; the per-component `.prompt.md` files are quick reference;
> this is the law.
>
> **Register:** product. **Three words:** precise · mechanical · honest.
> **Built for:** terminal-native expert developers reaching for an instrument,
> not a collaborator.

---

## 0 · Reading this document

Iris is not a web app wearing a terminal costume; it is a **monospace
character-cell interface** that we translate faithfully to the web. Every rule
below is stated in terminal terms first (cells, rows, glyphs) and then in its
CSS translation. When a rule and its translation seem to conflict, honour the
terminal intent.

The unit of measure is **one cell** — one monospace character width (`1ch`) and
one line of the terminal grid. "Two cells of indent" means `2ch`, not "about
16px". Vertical rhythm is measured in **blank lines**, not pixels.

---

## 1 · The pane — global anatomy

Iris is a **single vertically scrolling transcript column** framed by a quiet
**session bar pinned at the top** and a **fixed multiline composer pinned at
the bottom**. That is the entire chrome. There is:

- **no sidebar** — no file tree, no history rail, no agent avatar;
- **no top tab bar** — the session bar is one quiet row (location + context),
  not a toolbar;
- **no separate bottom status bar** — the runtime statusline lives *inside*
  the composer, below the input, so status and input are one object;
- **no floating toolbars, no FABs, no cards, no panels-beside-panels.**

The statusline is **split** across the two ends of the pane, and the two
halves are never merged onto one line again:

- **Session bar (pane top — "where am I / how full am I"):** `cwd ┊ git
  branch` left, the right-aligned context readout `CTX <used>/<cap>` + 10-dot
  meter right, over a soft (dim) hairline.
- **Composer statusline (pane bottom — "what am I running"):** mode · model ·
  effort · approval policy, below the input rows.

```
┌───────────────────────────── pane (one column) ─────────────────────────────┐
│  ~/iris-agent ┊ git main                      CTX 94k/300k ●●●○○○○○○○        │
│  ────────────────────────────────────────────  (session bar + soft hairline) │
│  <transcript — scrolls>                                                      │
│    user text                                                                 │
│    › assistant text                                                          │
│    THINKING ▸ …                                                              │
│    ▾ EXPLORE  src                       0.0s   (tool block — frameless)      │
│       Read  src/lib.rs           142 lines                                   │
│       ─────────────────────────────────────  (hairline footer rule)    │
│       DONE                              ↑1.4k ↓38 ┊ cache 16.8k ┊ ctx +0.9%  │
│    ●··· 0:13 ┊ ESC ┊ ↑177k ↓5.7k             (working indicator, inline)     │
│    ── 7.6s ┊ ↑18.2k ↓846 ───────────────────  (turn divider)                │
│                                                                              │
│  ────────────────────────────────────────────  (composer top edge — frame)  │
│  Give Iris a task...                                                         │
│  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌  (internal rule — lighter)     │
│  ◉ CODE ─ GPT-5.5 XHIGH ─ ◆ always-approve                                   │
└──────────────────────────────────────────────────────────────────────────────┘
```

**Shared measure.** Tool panels and the composer indent **2 cells** from the
pane edge and share **one width**. Transcript text (user + assistant) aligns to
a single **text column** (see §7). Nothing is centred; nothing is full-bleed
except a genuine overlay's scrim.

**Max width (web).** In a browser recreation the column caps at `--pane-max`
(900px) and centres in the viewport on the flat `bg`. In a real terminal it is
the terminal width.

**Vertical rhythm.** Exactly **one blank line** (`--block-rhythm`, 1.5rem)
separates every top-level block: user turn, assistant message, thinking block,
plan, notice, each tool block, the working indicator, and the turn divider. The
calm of the interface comes from **varying nothing else.** Never 0.5-line,
never 2-line gaps; never a gap that depends on block type.

---

### 1.1 Screen modes — pager & inline

The pane anatomy above is rendered by one of two backends
([ADR-0029](adr/0029-adopt-alt-screen-pager-tui.md)). Both render the same
logical `Screen` state; the design language is identical in both.

| Mode | Surface | Session bar | Scrollback |
|---|---|---|---|
| **Pager** (rich default once stable) | Alternate screen, full-frame ratatui `Terminal`, synchronized updates | Literally viewport-pinned (rows 0–1) | Iris-owned scroll offset; native scrollback unused |
| **Inline** (automatic fallback) | Scrollback-append terminal surface (ADR-0006) | Top of the rendered document; scrolls with history | Native terminal scrollback |

In pager mode the mouse is captured by default (wheel scrolls the Iris-owned
scrollback); Ctrl+T or `/mouse` toggles capture off to restore
terminal-native select/copy, and the composer statusline shows a dim
`○ mouse off` hint while off. Copy paths: native clipboard tools, then
OSC 52 (`/copy`).

Mode policy: `tui.altScreen = "auto" | "always" | "never"` in settings,
`--no-alt-screen`, `IRIS_NO_ALT_SCREEN=1`. `auto` selects the pager on plain
terminals and normal tmux; tmux control mode, Zellij, `TERM=dumb`, and
non-TTY stdio degrade to inline with a one-line notice. `--plain` remains the
ANSI-free text path. Detection failures degrade to inline, never to a broken
alt screen.

---

## 2 · Color

**Terminal-relative.** Every role binds to an ANSI named slot so Iris inherits
the user's own light/dark terminal theme. The hexes below are the **dark-mode
reference approximations** (from `src/ui/tui.rs`); `tokens/colors.css` is the
source of truth for the values and ships a light theme under
`:root[data-theme="light"]`.

### 2.1 Foundation (grey does the structural work)

| Role | Token | Dark hex | ANSI | Job |
|---|---|---|---|---|
| Background | `--iris-bg` | `#1a1a1f` | Reset bg | The entire canvas. Flat. |
| Surface | `--iris-surface` | `#323238` | `Rgb(50,50,56)` | Selection / active-row fill **only**. |
| Ink | `--iris-ink` | `#e6e6e6` | Reset fg | Default body text. |
| Border | `--iris-border` | `#808080` | Gray | Panel & composer frames. |
| Muted | `--iris-muted` | `#6b6b6b` | DarkGray | Metadata, hints, markers, elisions. |
| Stdout | `--iris-stdout` | `#b7b7bd` | — | SHELL program output (below the command). |

Grey carries the whole layout. If you can express a distinction with
weight/case/dim instead of a hue, do.

### 2.2 Signal (sparse, role-assigned)

| Role | Token | Dark hex | ANSI | Used for |
|---|---|---|---|---|
| Accent | `--iris-accent` | `#d78700` | orange | Active mode `◉`, running `●`, meter edge dot, warning `▲`. |
| Interactive | `--iris-interactive` | `#00afaf` | Cyan | Selection focus, inline code. |
| Link | `--iris-link` | `#5f87ff` | Blue | Links only. |
| Success | `--iris-success` | `#5faf5f` | Green | `◆` DONE/APPROVED, diff additions. |
| Danger | `--iris-danger` | `#d75f5f` | Red | `■` ERROR/DENIED, diff removals, stderr. |

### 2.3 Two laws of color

1. **Never color a whole panel or region.** Color is a point signal (a glyph, a
   word, one diff row's faint tone), never a fill behind content. The single
   permitted tonal fill is `--iris-surface` for a selected/active row.
2. **Never rely on color alone.** Every stateful thing pairs a **symbol + label**
   with its color, and the UI must be fully legible in monochrome. A red word
   with no `■` and no "ERROR" is a bug.

### 2.4 Diff tone

Additions/removals get a **whisper** of background — `color-mix` of the
success/danger role at ~10% into the pane bg — plus tinted text and a `+`/`−`
marker. The marker and text carry the signal; the tone only groups the hunk.
Never a saturated block.

---

## 3 · Type

**One family:** the user's terminal monospace. Web substitute: **JetBrains
Mono** (complete box-drawing coverage, even cell widths), loaded from Google
Fonts; swap the stack in `tokens/typography.css` for a house font or offline
build.

**There is no size axis.** The terminal has one cell size (`--fs-base`, 14px on
the web). Hierarchy is built from five levers, in this order of preference:

1. **Weight** — `400` body · `500` actor lines / active items · `700` labels & headings.
2. **Dim / bright** — muted grey recedes; ink advances; stdout sits between.
3. **Color** — only per §2 (sparse, always symbol-paired).
4. **Case** — UPPERCASE for structural labels only (see §11).
5. **The marker/symbol column** — a leading glyph is itself a level of hierarchy.

The `--fs-*` steps exist **only** so web chrome (specimen cards, README) has a
sane base. **Never introduce a larger font size to make something important in
the pane** — reach for weight, then case, then a marker.

**Line rhythm:** `--leading-base` 1.5 for prose/panels; `--leading-tight` 1.35
where density matters. Uppercase labels get `--tracking-label` (0.06em).

**Wrapping is semantic.** Break at spaces, `/`, `&&`, and token boundaries.
**Never** break an identifier, a path, or a decimal; **never** let a line
overflow a border. Continuation lines align under the content column, not the
marker (see §7, §8).

---

## 4 · Spacing & rhythm (exact)

| Token | Value | Meaning |
|---|---|---|
| `--cell` | `1ch` | One character width — the grid unit. |
| `--pane-indent` | `2ch` | Tool blocks & composer indent from the pane edge. |
| `--marker-gap` | `2ch` | Assistant `›` marker → its text. |
| body hang | `3ch` | Tool-block body/footer indent under the header (the spec's `2.5ch` snapped to the cell grid). |
| `--block-rhythm` | `1.5rem` | The one blank line between top-level blocks. |
| `--line` | `1.5em` | One line of vertical rhythm. |

**Golden rule:** inside a tool block every row is exactly **one** of
{ header · body · footer rule · footer } and **all rows share one width**. The
column discipline is the design: left edges (disclosure · TOOL · body indent ·
state label) and right edges (elapsed · op metas · diagnostics) form the two
rails that make the transcript scan as a table without drawing one.

---

## 5 · The symbol vocabulary (complete)

Iris has **no icon font, no SVG icon set, no emoji — ever.** Its entire "icon
system" is this closed set of Unicode glyphs rendered in the cell grid. Each
glyph has **exactly one job.** Do not introduce new glyphs; do not reuse one for
a second meaning.

```
STATE / ACTIVITY
  ◉  active / selected mode (orange)        ●  running · live LED (orange)
  ◆  done / approved (green)                ◇  preview / pending (muted)
  ■  error / denied (red)                   ▲  warning / review (orange)
  □  skipped / cancelled (muted)            ○  queued / empty meter slot (muted)

TRANSCRIPT
  ›  assistant message marker (muted)       ▋  live caret (orange, thinking)
  ▾  expanded disclosure                    ▸  collapsed disclosure
  •  markdown list bullet (muted)           1. ordered list marker (muted)

DIFF / TELEMETRY
  +  addition (green)                       −  removal (red — UNICODE minus, not ASCII -)
  ↑  input tokens                           ↓  output / generated tokens
  ┊  soft metadata separator (NOT ASCII |)  ─  rule / frame line / statusline separator

GIT / TASK (session bar + git console)
  ⇡  commits ahead of upstream              ⇣  commits behind upstream
  ±  uncommitted modification               [WT]  linked-worktree text tag (a label, not a glyph)

METER
  ●●●○○○○○○○  context meter — 10-dot LED strip (filled muted · edge orange · empty dim)

FRAME (box-drawing, square corners ONLY)
  ┌ ┐ └ ┘   corners        │  vertical        ─  horizontal        ├ ┤  tees
```

**Punctuation law:** use the ellipsis `…` (never `...`); use the Unicode minus
`−` for removals (never ASCII `-`); use `┊` as the soft separator (never ASCII
`|`). A glyph is added only when it carries meaning — do not decorate.

**Git/task senses (exact, one job each):**

- `⇡` / `⇣` — ahead/behind the **last-fetched** upstream, git console only.
  `↑`/`↓` remain token telemetry ONLY; never reuse them for sync state.
- `±` — uncommitted modification relative to committed state: diff modified
  rows, the session-bar dirty count, and user-attributed dirty files. One
  meaning everywhere.
- `◇` — pending / not yet settled ("exists, awaiting acceptance"): tool
  previews AND unsettled Iris task changes (ADR-0028). One meaning.
- `▲` conflicts / `■` detached — the existing warning/error roles paired with
  a label (`▲2`, `■ detached @ 46b104`), never color alone.
- `WT` — a boxed **text tag**, not a glyph, marking a linked worktree.
  Staged/untracked counts are **words** (`1 staged · 3 untracked`); `+`/`○`
  keep their single jobs.
- TAB inside a create input toggles the creation **target** (branch ⇄
  worktree). Distinct from the SlashMenu's tab-to-accept, which is a
  completion context; a target toggle never completes text.

The only raster/vector brand asset is the hero banner (`assets/hero-*.svg`),
itself a monospace specimen (LED strip + `›` + tagline, one orange accent).

---

## 6 · Elevation, borders, motion, transparency

- **Flat by construction.** No z-layers in the transcript; `--radius: 0`
  everywhere (square corners are intrinsic to box-drawing). No decorative
  shadows, no faux-3D, no gradients, no textures, no images (except the hero).
- **Depth is structural.** Tool output is unboxed text like the rest of the
  transcript; structure comes from the block grammar (header · hanging body ·
  hairline footer) and its two alignment rails, not from a frame. The composer
  keeps its frame — it is the only hard chrome on screen.
- **The one permitted shadow** is a faint cast under a genuine overlay
  (`--overlay-shadow`), which is the real top layer. Overlays sit on a
  low-opacity black scrim; the pane is otherwise fully opaque. No blur, no glass.
- **Motion is almost nil.** Only two live motions exist: the **LED-chase
  working indicator** (`●··· → ·●·· → ··●· → ···●`) and the **edge-dot pulse**
  on the context meter / running symbol at high usage. No braille spinners, no
  rainbow meters, no easing-heavy transitions. Both degrade to a **static
  readout** under `prefers-reduced-motion: reduce`.
- **Interaction states are quiet.** Hover/selected rows in overlays use the
  `surface` fill — never a colored left-border accent. Focus is the cyan
  interactive role. State changes are reported by the symbol vocabulary, not by
  shrink/scale/bounce.

---

## 7 · Transcript grammar — conversation

Natural-language conversation is **unboxed and light.** Chrome (frames) is
reserved for mechanical tool events (§8). The transcript text column is defined
by the assistant marker: marker width (`1ch`) + `--marker-gap` (`2ch`).

### 7.1 User message
Plain transcript text on the text column. **No `›` marker, no USER label, no
border, no role card, no bubble, no avatar.** One blank line separates turns.

### 7.2 Assistant message
The muted `›` marker sits one column left of its text; **wrapped lines align
under the text, not the marker.** Never boxed; never an "AGENT" label. Content
is rendered through the **markdown grammar** (§7.3).

Voice inside: terse, factual, present-tense reports of *what happened* — "Done;
emit() now budgets before sending. The diff is above." Never "I think", "I'll go
ahead and", "Let me". No enthusiasm performance, no emoji.

### 7.3 Markdown grammar (assistant rich text)
Iris speaks prose but carries structure. GFM is rendered in the terminal idiom —
hierarchy from weight/case/color/marker, **never a size jump**:

| Construct | Rendering |
|---|---|
| Heading `#`–`####` | Bold ink, no size change. `#` (h1) gets uppercase + label tracking. |
| **Bold** | `--fw-bold` ink. |
| *Italic* | Slanted (JetBrains Mono italic). |
| `Inline code` | Cyan interactive, monospace (already monospace — color is the cue). |
| `[link](url)` | Link blue, **dotted** underline, 2px offset. |
| Fenced ```` ``` ```` | `CodeBlock`: quiet **left rail**, muted `lang · file` caption, ink body, horizontal scroll. **Never boxed**. |
| List `-`/`*`/`+` | Muted `•` marker column, hanging indent. |
| List `1.` | Muted right-aligned `1.` marker column. |
| Blockquote `>` | Muted **left rail**, muted text. |
| Rule `---` | A single muted `─` line (50% opacity). |
| Table | Aligned monospace columns, **bold header**, one `─` separator row, ink body. No vertical rules. |

### 7.4 Thinking block
The agent's raw reasoning. Reasoning is internal, secondary, verbose, and **not
a mechanical event**, so it gets **no chrome.** It is the most recessive thing in
the pane: a muted `THINKING` label, dim-grey body behind a quiet **left rail**
(a rule, never a box), and generated-token telemetry. Folds by default
(progressive disclosure); `ctrl+o` / header toggles `▾`⇄`▸`. Live reasoning
pulses (`●` in the label, `▋` caret at the tail); finished reasoning may collapse
to a line + token count. Short reasoning is shown whole and is not foldable.

### 7.5 Plan list
The agent's task checklist. **Unboxed** (narration, not a tool event): a muted
`PLAN` label with a `done/total` count, then one row per step carrying its state
as a glyph — `◆` done (recedes, muted text) · `●` active (orange, pulses,
bright+bold) · `○` pending (muted) · `□` skipped (muted). Never color alone; a
step may carry a muted trailing note.

### 7.6 Notice (system message)
A runtime event that is neither a tool call nor the assistant: context
compaction, interrupt, undo, connection retry, rate-limit, model switch.
Unboxed and quiet. State is a glyph + optional label: `┊` info (muted text) ·
`◆` success · `▲` warning · `■` error · `□` cancelled. Prefer one line; use a
muted `meta` for counts and a caption `hint` for a keybind (e.g. `ctrl+r to
undo`).

### 7.7 Working indicator
An **inline** LED-chase readout shown while the agent runs. Never framed, never
a braille spinner, one line:

```
●···  1:27 ┊ ESC ┊ ↑177k ↓5.7k
```

The lit cell bounces across a 4-cell strip. One blank line above/below when
adjacent to other blocks. Telemetry (`↑`/`↓`) and the `ESC` hint are optional.

### 7.8 Turn divider
A quiet unboxed rule rendered **after a tool-backed agent turn** (not after
purely conversational turns). Compact elapsed + optional token telemetry with
`┊` separators; **never** `T+`. One blank line above and below.

```
── 7.6s ┊ ↑18.2k ↓846 ───────────────────────────────────
```

---

## 8 · Tool-block grammar — the frameless families

The **tool block** is Iris's primary structured-output primitive. It is
**frameless**: no border, no background, no header/body separator — unboxed
text, like the rest of the transcript. Every block is **header · body ·
footer**, stacked, sharing one width at the 2-cell tool indent. The transcript
families are **EXPLORE / SHELL / EDIT** (plus the docked APPROVAL review,
§8.5). Never invent another; never render standalone `READ` / `GREP` / `LS`
panels.

### 8.1 Shared block grammar
```
▾ TOOL  meta                                                        ELAPSED
   <body — hangs 3 cells under the header, unmounts when collapsed>
   ─────────────────────────────────────────────────────────────
   STATE  [family extras]              ↑sent ↓recv ┊ cache <n> ┊ ctx <Δ%>
```
**Header** — disclosure `▾`/`▸` (muted) · bold uppercase family label · muted
meta (a path, scope, or the shell command), truncating with `…` · right edge
carries **only the elapsed time** (omitted for a pending `preview`). No state
symbol in the header.

**Footer** — the block's last row, always visible, opened by a muted hairline
rule from the body indent to the right rail. Left edge: the **state as label
only** (`DONE` / `ERROR` / `PREVIEW` / `RUNNING` / `CANCELLED`), bold
uppercase, colored by state — **no glyph**. After it, `┊`-joined family
extras (EDIT counts + note, SHELL `EXIT <code>` + result meta). Right-bound:
the optional token diagnostics cluster, all muted, honest (rendered only when
measured). The `┊` law: only BETWEEN sibling fields, one space each side,
never leading/trailing, never after the state label — fields are joined
programmatically so a missing field can never leave a dangling `┊`.

**Disclosure** — binary, whole-block. Expanded (`▾`) = header + body +
footer; collapsed (`▸`) = header + footer, exactly two rows, body
**unmounted** — no partial preview, no elision affordance. **Compact by
default**: every foldable block **arrives collapsed** regardless of body
size (the two rows still answer *what ran · on what · how long · outcome ·
cost*). Two exceptions: a **running** block stays expanded on its bounded
live tail (it collapses when it finalizes unless the user explicitly
expanded it), and a **pending EDIT preview** (`◇ PREVIEW`) arrives expanded
for review (it collapses once applied). `ctrl+o` toggles **all** foldable
blocks at once — tool blocks and thinking rails: if any is collapsed it
expands them all, otherwise it collapses them all. A **click on a block's
header row** toggles that one block. State is per-block; an explicit user
expand/collapse survives the block's in-place rebuilds.

### 8.2 EXPLORE — read / grep / list / find
The **single container** for every read-side op. Each op is **one row**:
```
VERB  target [code][after]                                    meta(count)
```
- `verb` (fixed 5-cell column, medium weight): `Read` · `Grep` · `List` · `Find`.
- `target` ink path; `code` cyan (a grep pattern); `after` muted (` in src/…`).
- `meta` muted count, right-bound at the block's right rail (`142 lines`,
  `3 matches · 2 files`).

Never break a read op into its own block — batch them here. The EXPLORE footer
is state + diagnostics only (no family extras).

### 8.3 SHELL — command execution
Header meta is the command. Body line types, in the recessive order below (the
command is brightest, output recedes):

| `type` | Rendering |
|---|---|
| `cmd` | Bright ink, medium weight, quiet muted `$ ` prompt (non-selectable). |
| `out` | Recessive **stdout** grey, below the command. |
| `err` | **Danger** red (stderr). |
| `note` | Muted aside. |

A live command streams a bounded tail in the body (with an honest
`… N earlier lines hidden` marker) and has **no exit field yet**. A finished
command reports its status in the **footer**: `EXIT <code>` (bold, uppercase,
muted) then the honest result meta as a sibling field —
`DONE  EXIT 0 ┊ 142 passed` / `ERROR  EXIT 101 ┊ cargo bench failed`. The
footer state comes from the result (`exit 0` → done, else error); an unknown
exit status is omitted, never guessed.

### 8.4 EDIT — mutation & diff preview
**One canonical body:** the wrapped **block diff** (`DiffBlock`) for every file
type (code, prose, config, markdown). The footer carries the counts as ONE
field (`+n` add-ink, `−n` del-ink, 1ch apart) plus a muted note (`new file`).
Use `state="preview"` (**no elapsed**) for a pending apply; `state="done"`
once applied.

### 8.5 APPROVAL — authorization review
Compact. The **header carries the decision** (`▲ REVIEW` · `◆ APPROVED` · `■
DENIED`). The body carries:
1. the **action** (a `$ command` if `shell`, else prose), optionally with
   `+add/−del` on the right;
2. optionally the **diff** being authorized (`DiffBlock`, under a hairline);
3. a **reason** — muted, or danger-toned when the action is denied/destructive;
4. when `review`, a hairline-ruled **decision affordance** of `┊`-separated
   key hints (`y approve ┊ n deny ┊ e edit ┊ a always`).

### 8.6 Diff rendering (`DiffBlock`) — shared by EDIT & APPROVAL
Columns: **line number** (right-aligned, muted, non-selectable) · **marker**
(1 cell) · **content** (wraps; continuations align under content). Markers:
`+` addition (green + faint add-tone bg), `−` removal (red + faint del-tone bg,
**Unicode minus**), `±` modified (accent), ` ` context (plain ink). Tone + text
+ marker together — never color alone.

---

## 9 · Session chrome — the session bar & the composer

The statusline is split across the pane: the **session bar** (top) answers
"where am I / how full am I"; the **composer statusline** (bottom) answers
"what am I running". The two halves are never merged onto one line again.

### 9.1 Session bar (pane top)

A quiet, always-visible row pinned above the transcript (the transcript
scrolls beneath it), with one soft hairline under it (dim `─` repeat — NOT the
full border weight; visibly lighter than the composer's top edge). No
background fill, no color bar.

```
~/iris-agent ┊ git main                      CTX 94k/300k ●●●○○○○○○○
────────────────────────────────────────────────────────────────────
```

- **Left:** `<cwd> ┊ git <branch> [state cluster]` — cwd in body ink, `┊` and
  `git <branch>` dim. Paths middle-ellipsize (never break; the project name
  survives). In a worktree, the worktree path is the cwd and a dim `[WT]` tag
  follows the cluster.
- **State cluster** (mutually exclusive base states, precedence order):
  1. unmerged `▲N` (orange) — overrides everything until resolved;
  2. task-partitioned `±N ◇M` — `±N` orange = user-attributed dirty files,
     `◇M` dim = Iris-unsettled ledger files; either half omitted at zero;
  3. plain dirty `±N` (orange) — one number, no task;
  4. clean — no glyph. Silence is the signal.
  Detached HEAD renders `■ detached @ <short-sha>` in place of the branch. No
  `⇡⇣` at rest — sync is git-console detail.
- **Right, right-aligned:** `CTX <used>/<cap>` + the 10-dot LED meter. `CTX`
  and `/<cap>` dim; `<used>` body ink. Unknown context window: `CTX <used>`
  with no meter.
- **Narrow widths, drop in order:** meter → `/<cap>` → counts (`±2 ◇3` →
  `±`) → `WT` tag → whole git segment → middle-truncate the cwd harder.
  Minimum form: cwd alone.

#### 9.1.1 SessionBar disclosures — the directory tree & the git console

Two momentary dropdowns share one slot under the bar: the **directory tree**
(from the cwd; `/tree`, or `@` as the first character of an empty composer —
opens straight into filter mode) and the **git console** (from the git
segment; `ctrl-g` or `/git`). They are **top chrome, not overlays**: rows
render between the bar and its soft hairline (which becomes the closing rule),
pushing the transcript down — plain `bg`, no box, no shadow, no scrim. At most
one is open; opening one closes the other; a docked modal or approval closes
both. A dim `▾ ` prefixes the open dropdown's segment only while it is open.
Height caps at 16 rows or ⅓ of the pane.

- **Focus:** `Editor < Palette < SessionMenu < Modal`. While open the dropdown
  owns keys; `esc` closes it and never reaches the turn-interrupt path. The
  **list-state law**: while a LIST has focus there is no free typing —
  single-letter commands (`a r n w s /`) are legal only there; any INPUT row
  (filter, create) makes printable keys text, always.
- **While a turn runs** dropdowns open as READOUTS: rows dim, every mutating
  key is a no-op, and the footer reads `● agent running ┊ read-only — actions
  return when idle ┊ esc`.
- **Git console** = the settlement surface for ADR-0028 tasks: a dim status
  line (`main → origin/main ┊ ±2 yours · 1 staged · 3 untracked ┊ ⇡2 ┊ stash
  1 ┊ 3h ago`), a TASK group (`a accept ┊ r roll back` — `r` swaps in the
  restore-point sublist from `restore_points()`), a SWITCH list (≤8 recent
  branches, `[WT]` rows redirect to "open session there"), and a WORKTREES
  board with `◇ unsettled · <age>` badges. Switching with dirt confirms first
  (settle / stash / carry); conflicts disable switching. `n`/`w` create a
  branch/worktree from the selected base — TAB toggles the target, validation
  gates `↵`, and the resolved worktree path (config `worktreeRoot`, default
  `../wt`) is always visible before create. Settlement goes through the
  existing `GitSafety` API only.
- **Directory tree**: breadcrumb (parents dim, clickable re-root up), 2-cell
  indent per level, `▾`/`▸` disclosure on dirs — no box-drawing tree guides.
  Attribution metas from the task partition: `◇ iris` dim, `± yours` orange,
  `◉ open` for the composer-referenced file. `↵` on a file inserts
  `@<relative-path>` into the composer; `/` filters flat (parent path as dim
  meta). Data: `git ls-files --cached --others --exclude-standard`, plain
  readdir outside a repo; 500 visible rows, then a dim `… N more` row.
- These are **disclosures, not sidebars**: invariant #1 stands — nothing
  persistent, nothing beside the transcript.

### 9.2 The composer

**Always present at the bottom. Never hidden, revealed, or collapsed** — there
is no show/hide mechanic anywhere. Row order, top → bottom:

```
────────────────────────────────────────────  ← top edge: full border-frame hairline
Give Iris a task...                           ← input rows (1 → 8)
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌  ← internal rule: lighter hairline
◉ CODE ─ GPT-5.5 XHIGH ─ ◆ always-approve     ← bottom statusline
```

Exactly this **two-weight rule**: the top edge (separating composer from
transcript) is the full `border-frame` hairline; the rule between the input
and the statusline is a lighter internal hairline (the same soft weight panels
use internally). There is no other chrome option.

### 9.3 Bottom statusline (the composer's last row)
`◉ MODE ─ MODEL EFFORT ─ <policy-symbol> <policy>`. The `◉` is orange; `MODE`
bold uppercase; ` ─ ` dim separators; model name is an **underlined button**
(opens the model `Picker`); effort is muted. The approval-policy segment is
state symbol + label, never color alone:

| Posture | Segment |
|---|---|
| always-approve | `◆` green + dim label |
| on-request | `▲` orange + dim label |
| read-only | `■` red + dim label |
| off (approvals disabled) | `○` dim + dim label |

**Narrow widths, drop in order:** policy → effort → minimum `◉ CODE ─ MODEL`.
cwd/branch/context NEVER appear here — they live on the session bar.

### 9.4 Input row
A single editable row directly beneath the top edge, growing **1 → 8 rows** as
the user types. Caret is the orange accent. Placeholder uses exact product
casing: `Give Iris a task...`. Submit on `↵`; `shift+↵` for newline.

### 9.5 Command palette (`/`)
Typing a leading `/` opens the `SlashMenu` **above** the input: an overlay list
of `command  —  one-line description`; `↑`/`↓` navigate, `↵`/`Tab` accept,
`esc` dismisses. The highlighted row uses the `surface` fill (no accent border).
Canonical commands: `/model` · `/diff` · `/undo` · `/compact` · `/clear` ·
`/copy`.

### 9.6 File reference (`@`)
`@` references a workspace file (a path completion). Same overlay idiom.

---

## 10 · Overlays

Overlays are the **genuine top layer** — the only place a shadow appears. All
share: `frame-overlay` border, `surface-overlay` fill, `--overlay-shadow` cast,
an optional uppercase title row, and rows whose highlight is the `surface` fill.

- **SlashMenu** — command palette (§9.3).
- **Picker** — model switcher, settings, scoped-models, login provider list.
  Rows: `[◉ if active] label … meta hint`. `◉` marks the current selection.
- **HelpOverlay** — the `?` cheatsheet: grouped key→action rows (keys in ink,
  actions muted, quiet uppercase group headings). No color, no icons.

---

## 11 · Casing & content

- **Sentence case** for all prose.
- **UPPERCASE** is reserved for structural labels: tool families
  (`SHELL`/`EXPLORE`/`EDIT`/`APPROVAL`), states (`DONE`/`RUNNING`/`ERROR`/…),
  mode (`CODE`), section labels (`PLAN`/`THINKING`), and `EXIT`. **Never**
  uppercase for emphasis in prose.
- **Numbers are honest.** Token telemetry (`↑177k ↓5.7k`), durations (`7.6s`,
  `1:27`), counts — shown compactly and only when real. Never assert savings the
  runtime hasn't measured.
- **Brevity.** Hints are short and inline (`↵ to send • shift+↵ for new line • /
  for commands`). Placeholders use exact product casing.
- **Emoji: none, ever.** State is carried by the glyph vocabulary.
- **Progressive disclosure.** Minimal at a glance; complete and structured on
  demand (`ctrl+o`). Nothing important is hidden; nothing trivial is shouted.

---

## 12 · Accessibility & the monochrome test

- **The monochrome test is a hard gate.** Desaturate the whole pane: every state
  must still be unambiguous from symbol + label + position. If a state is only
  distinguishable by hue, it is broken.
- Live regions: the working indicator is `role="status"`; the context meter is
  `role="meter"` with `aria-valuenow`; decorative glyphs are `aria-hidden`.
- All motion respects `prefers-reduced-motion`.
- Contrast: ink on bg and muted on bg both clear the terminal-legibility bar in
  both themes; stdout grey is deliberately recessive but still readable.

---

## 12.5 · The start page

Shown when Iris launches interactively with no task and no resume target —
before any transcript exists. Same pane chrome (session bar on top, composer
on bottom, both live), with the launcher centered in the empty transcript
area. Entering a session replaces the launcher with the normal transcript;
nothing else changes — that is the point of the shared chrome. On the start
page the session bar shows the launch cwd/branch and an empty meter
(`CTX 0/<cap>`, all `○`).

```
~/demo ┊ git main                                     CTX 0/300k ○○○○○○○○○○

                        ○ ○ ○ ● ○ ○ ○ ○ ○ ○ ○ ○        ← IrisMark (animated)

                        ◉ New session ················ ctrl-n
                          Resume session ·············· ctrl-r
                          Settings ····················· ctrl-,
                          Quit ························· ctrl-q

──────────────────────────────────────────────────────────────────────
Give Iris a task...
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
◉ CODE ─ GPT-5.5 XHIGH ─ ◆ always-approve
```

**IrisMark.** The logo IS an LED strip — no ASCII art, no figlet wordmark, no
pictorial glyph. One row of 12 dots (`●`/`○` cells, single-spaced), centered. A
single lit orange head sweeps back and forth (ping-pong: reverses at the ends,
never wrapping), advancing one dot per ~130ms tick, with a 2-dot comet trail
behind the travel direction (trail-1 non-bold orange, trail-2 dimmest; head
bright orange). All other dots are dim `○`. It reuses the working indicator's
tick machinery: it stops when the terminal is unfocused, and under
`IRIS_REDUCED_MOTION` it holds a single static lit dot at the center.

**Launcher.** A keyboard-navigable list (~44 columns, centered, one blank row
below the mark) in the house picker idiom — NO hairline dividers between rows:
a 1-col `◉` orange marker on the selected row, the action label (bold when
selected), a dim dotted leader, and the right-aligned dim key hint. The
selected row gets the `surface` fill across the menu width. `↑`/`↓` move the
selection (wrapping), `↵` activates, and the listed `ctrl-` chords activate
directly. The composer input stays live: typing a task and pressing `↵`
starts the session with it.

---

## 13 · Invariants (golden tests — a build MUST satisfy)

1. **One column.** No sidebar, no tabs, no separate status bar (the split
   statusline lives on the session bar and inside the composer).
2. **One blank line** between every top-level block. No other gap value.
3. **Shared measure.** Panels + composer share one width and a 2-cell indent;
   transcript text shares one column.
4. **Block rows** are each exactly one of {header·body·footer rule·footer} and
   all share one width; no row overflows the block's rails.
5. **Four tool families only.** No standalone READ/GREP/LS/DIFF panels.
6. **Chrome is for tools.** Conversation, thinking, plans, and notices are never
   boxed. Boxes are never used for prose.
7. **Square corners always** (`--radius: 0`).
8. **State = symbol + label + color**, never color alone; the pane passes the
   monochrome test.
9. **One type size.** Hierarchy never uses a larger font in the pane.
10. **Closed symbol set.** No glyph outside §5; `…`/`−`/`┊` (not `...`/`-`/`|`);
    no emoji.
11. **Composer is unconditional.** No show/hide/reveal/collapse mechanic.
12. **Motion** is only the LED chase (working indicator + IrisMark) + edge
    pulse, all reduced-motion safe.

---

## 14 · Anti-patterns (do NOT)

- ✗ A role card / bubble / avatar for user or assistant messages.
- ✗ A colored left-border accent on active rows (use the `surface` fill).
- ✗ Boxing a code block, a plan, a notice, or tool output — nothing in the transcript is boxed.
- ✗ A braille spinner, a rainbow/percentage meter, or an animated progress bar.
- ✗ A larger font, all-caps prose, or bold-for-emphasis to signal importance.
- ✗ Emoji, gradients, rounded corners, drop shadows in the transcript, glass/blur.
- ✗ ASCII `|` separators, ASCII `-` removals, or `...` ellipses.
- ✗ Asserting efficiency/savings the runtime has not measured.
- ✗ A fifth tool family, or a standalone READ/GREP/LS/DIFF panel.
