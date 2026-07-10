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
│    › user text                          (the one marked turn — §7.1)         │
│    assistant text                       (the agent speaks unmarked — §7.2)   │
│    ▸ THINKING                           ↓2.4k 12s   (rail — shares the grid) │
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
a single **text column** (see §7). Nothing is full-bleed: the docked overlays
(§10) inset to the same measure, never a full-screen scrim. The only centred
surface is the start-page launcher (§12.5).

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
| Success | `--iris-success` | `#5faf5f` | Green | `◆` DONE, diff additions. |
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

**Measure.** Prose is read, not scanned — a line that runs the full width of an
ultrawide pane loses the reader on the way back. So **prose wraps at
`min(pane, 96)` columns**: assistant paragraphs / list items / headings,
thinking bodies, notices, plan-step notes, and user message bodies rag at the
measure while the marker, rail, and indent stay exactly where they are (nothing
is centered; the right side simply rags). **Mechanical output uses the full
pane** — fenced/indented code, tool bodies, diffs, tables, rules/dividers, and
session chrome are column-aligned and must not reflow. The measure is a **print-
time** decision: a printed block reflects the terminal it was printed into and
is never retroactively reflowed (rows are immutable in scrollback). On any pane
≤ 96 columns the measure is a no-op.

---

## 4 · Spacing & rhythm (exact)

| Token | Value | Meaning |
|---|---|---|
| `--cell` | `1ch` | One character width — the grid unit. |
| `--pane-indent` | `2ch` | Tool blocks & composer indent from the pane edge. |
| `--marker-gap` | `2ch` | User `›` marker → its text (the marker occupies the gutter; the body hangs on the text column). |
| body hang | `4ch` | Body indent: one 2-cell step under the header **label**. Every block's body — tool, thinking rail, and a user turn's own text — lands on this ONE shared text column. |
| `--block-rhythm` | `1.5rem` | The one blank line between top-level blocks. |
| `--line` | `1.5em` | One line of vertical rhythm. |

**The indentation ladder (one rule, three steps).** Indentation is hierarchy,
and it steps in units of 2 cells, the same everywhere:

- **col 2 — the gutter:** a row's identity glyph. A foldable block's disclosure
  `▾`/`▸` (tool *and* thinking) and a user turn's `›` live here; nothing else.
- **col 4 — the label/marker column:** tool & thinking **labels**, tool footers,
  the tool block's `┊` body spine (§8.1) and the thinking `┊` body rail, and the
  user's `›` marker.
- **col 6 — the text column:** *every* body — user prose, agent prose, tool
  output, reasoning — hangs here, one step under its header/marker.

**One right rail.** All right-aligned readouts — tool `elapsed`, footer
diagnostics, and the thinking-rail telemetry (`↓tokens elapsed`) — align to a
single vertical at the block's right edge (`width − pane-indent`). The reasoning
readout is not inset further than the tool elapsed; if they don't line up, it is
a bug. Tool headers and the reasoning rail share ONE geometry builder so the two
cannot drift.

**Golden rule:** inside a tool block every row is exactly **one** of
{ header · body · footer rule · footer } and **all rows share one width**. The
column discipline is the design: left edges (disclosure · label · body · state
label) and the single right rail (elapsed · op metas · diagnostics) make the
transcript scan as a table without drawing one.

---

## 5 · The symbol vocabulary (complete)

Iris has **no icon font, no SVG icon set, no emoji — ever.** Its entire "icon
system" is this closed set of Unicode glyphs rendered in the cell grid. Each
glyph has **exactly one job.** Do not introduce new glyphs; do not reuse one for
a second meaning.

```
STATE / ACTIVITY
  ◉  active / selected mode (orange)        ●  running · live LED (orange)
  ◆  done / success (green)                 ◇  preview / pending (muted)
  ■  error / denied (red)                   ▲  warning (orange)
  □  skipped / cancelled (muted)            ○  queued / empty meter slot (muted)

TRANSCRIPT
  ›  user message marker (ink) — the one   ▋  live caret (orange, thinking)
     marked turn; the agent is unmarked
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
  ▏▎▍▌▋▊▉█    flow-meter fill — left-anchored eighth-block ramp (bright accent)

FRAME (box-drawing, square corners ONLY)
  ┌ ┐ └ ┘   corners        │  vertical        ─  horizontal        ├ ┤  tees
```

**Punctuation law:** use the ellipsis `…` (never `...`); use the Unicode minus
`−` for removals (never ASCII `-`); use `┊` as the soft separator (never ASCII
`|`). A glyph is added only when it carries meaning — do not decorate.

**Meter marks (exact):** `·` is the **shared unlit cell** — the LED chase's
dark cells and the flow meter's unlit cells speak one vocabulary: a slot that
could light, dark right now. The flow meter (§7.7) fills with the left-anchored
eighth-block ramp ` ▏▎▍▌▋▊▉█`, and `▏` doubles as its **peak tick** (dim, in an
unlit cell): the decaying high-water mark of the last burst — a sanctioned
double duty, defined here so it stays the only one.

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
- **No shadows anywhere.** Overlays (§10) are docked, **frameless** menus that
  reserve rows above the composer and shift the editor down — not a floating
  layer over the pane. No cast shadow, no scrim, no blur, no glass; the pane is
  flat and fully opaque throughout. The composer's top edge is the only frame.
- **Motion is physics, and it is quantized.** Every sanctioned motion is a
  discrete step on the loop's tick grid — machines step, they do not ease. The
  closed set:
  1. the **LED-chase working indicator** (`●··· → ·●·· → ··●· → ···●`) and the
     IrisMark's idle ping-pong sweep — the only *looping* motions, and they run
     only while something is genuinely live;
  2. the **edge-dot pulse** on the context meter / running symbol at high usage;
  3. the **power-on lamp test** (§12.5) — the start page's one-shot boot: the
     strip fills two LEDs per tick, holds all-lit for two ticks, releases. Runs
     once, on the start page only, and any key completes it instantly;
  4. the **detent flash** — when a bottom-statusline segment changes (model,
     effort, approval policy), the context meter's lit-LED count moves, or a
     settings-panel control clicks to a new position (§10.1), the changed
     element alone acknowledges it for two ticks, then settles: a newly lit
     LED renders **bright**; LEDs that go dark (compaction reclaiming
     capacity) hold a dim `●` after-image — **the exhale** — before settling
     to `○`; when growth and shrinkage land in the same tick the bright flash
     wins. The mechanical acknowledgment that a switch clicked into a new
     position. Never fires from startup initialization (it is armed only once
     the first frame settles), so a flash is always news;
  5. the **flow meter** — the working indicator's 6-cell display-stream inflow
     bar (§7.7): instant attack, quantized release (4 quanta per tick), and a
     peak tick that holds five ticks then decays one quantum per tick. Live
     only while the stream is — it exists only on the running indicator's
     line, resets with the turn, and vanishes with it;
  6. the **escapement** — the live streaming tails (the assistant active tail
     and the reasoning stream, §7.4) advance in **word-quantized steps** on the
     same tick grid, never in raw network bursts: a tiny bounded buffer
     releases a governed share of its backlog per beat (`pending/4` bytes,
     clamped between about one word and about five, extended to the next word
     boundary; CJK/no-boundary text falls back to the char-snapped share) — so
     the cadence **tracks arrival like a hand at the keys**: it speeds up when
     the stream runs hot, eases off as it thins, and never gulps a sentence.
     Steady-state lag is ~4 beats (~400 ms); a pathological burst (> ~1 KB in
     one delta) fast-forwards at half-the-buffer per beat rather than lag
     unboundedly. It **flushes instantly** on
     stream end, provider turn completion/cancel/error, an approval gate
     opening, session reset, and entering reduced motion — the machine never
     withholds against a decision. The committed-line pipeline (collector →
     holdback → paced commit) is fed from the same drained output as the tail,
     so pacing changes *when* a word shows, never *what* the finished message
     says.
  No braille spinners, no rainbow meters, no easing, no fades, no ambient
  motion. The live-reasoning `●` lamp (§7.4) is a **state light, not a motion** —
  a static glyph, either lit (receiving) or dark (settled) — so it adds no new
  entry to this closed set. Everything above degrades to its **static settled
  state** under `prefers-reduced-motion: reduce` / `IRIS_REDUCED_MOTION` — for the
  escapement, reduced motion is a **pass-through**: streamed text renders on
  arrival, the raw truth.
- **Interaction states are quiet.** Hover/selected rows in overlays use the
  `surface` fill — never a colored left-border accent. Focus is the cyan
  interactive role. State changes are reported by the symbol vocabulary, not by
  shrink/scale/bounce.

---

## 7 · Transcript grammar — conversation

Natural-language conversation is **unboxed and light.** Chrome (frames) is
reserved for mechanical tool events (§8). The transcript text column is the
shared body column (§4): the `›` marker width (`1ch`) + `--marker-gap` (`2ch`)
past the pane indent — the same column tool and reasoning bodies hang on.

### 7.1 User message
**The one turn the transcript marks.** An ink-weight `›` sits in the gutter (col
2) on the first line of the turn; the body hangs on the shared text column, and
**wrapped lines align under the text, not the marker.** Only the first line is
marked — a multi-line ask reads as one block under one `›`. The marker is the
whole treatment: **no USER label, no border, no role card, no bubble, no
avatar.** Monochrome-safe — marker + position carry it, never color. Why mark the
user and not the agent? The agent is the transcript's dominant voice (messages,
tools, reasoning); marking *it* would decorate the default. The user's turns are
sparse, and the `›` is the anchor the eye jumps to — "what did I ask?" One blank
line separates turns.

### 7.2 Assistant message
**The agent speaks unmarked.** Its body sits on the shared text column with a
blank gutter — no `›`, never boxed, never an "AGENT" label. Content is rendered
through the **markdown grammar** (§7.3). (Historically the `›` marked the
assistant; it now marks the user, §7.1.)

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
(the `┊`, never a box), and generated-token telemetry. Its **header shares the
tool block's geometry** (§4, §8.1): the disclosure `▾`/`▸` sits in the gutter
(col 2), the label on the label column (col 4), and the telemetry
(`↓tokens elapsed`) on the single right rail — so reasoning and tools scan on one
grid, and the readout is never inset further than a tool's elapsed. Only the
muted label tone and the `┊` body rail (at col 4, its text hanging at col 6) mark
it as recessive. Folds by default (progressive disclosure); `ctrl+o` / header
toggles `▾`⇄`▸`. While reasoning streams the header carries a **static orange
`●` lamp** after the label (lit = receiving — a state light, not a motion; the
moving text and caret carry the liveness) and a **live elapsed** readout on the
right rail (the live counterpart of the settled `↓tokens elapsed`, and the only
number honestly available live). Its live body is a bounded **tail window** — the
last four wrapped rows on the `┊` rail, the bottom row ending in the `▋` caret at
the stream edge — under a single honest `┊ … +N rows` elision counting the rows
currently hidden (a readout, not a button). The live text feeds through the
escapement (§6 motion 6), so the caret steps evenly in word-quanta on the tick
grid instead of jumping on network bursts. `ctrl+o` opens the full live stream
(window ⇄ full, remembered for the live phase only); a live trace of ≤ four rows
shows whole and is not foldable. On commit the lamp drops and the block settles
to the standard fold state (the caret exists only while printing). Finished
reasoning may collapse to a line + token count. Short reasoning is shown whole and
is not foldable (the arrow drops, but the gutter stays so the label holds its
column).

### 7.5 Plan list
The agent's task checklist. **Unboxed** (narration, not a tool event): a muted
`PLAN` label with a `done/total` count, then one row per step carrying its state
as a glyph — `◆` done (recedes, muted text) · `●` active (orange, pulses,
bright+bold) · `○` pending (muted) · `□` skipped (muted). Never color alone; a
step may carry a muted trailing note.

### 7.6 Notice (system message)
A runtime event that is neither a tool call nor the assistant: context
compaction, interrupt, undo, connection retry, rate-limit, model switch. Unboxed
and quiet. State is a glyph: `┊` info (muted) · `■` error (red) — the info glyph
is the same soft rail the reasoning trace uses, never a color alone.

A notice is a **left-rail aside**, not a floating tick. It renders on the text
column (`┊` at col 4, message at col 6), **word-wraps** (never truncates), and an
info notice re-emits the `┊` rail on every continuation row — byte-for-byte the
reasoning body rail (§"reasoning rail"). An error leads its first line with `■`
and hangs its continuation under the message.

**A run of notices shares one rail.** When several fire back-to-back (a
compaction's runtime event plus the `/compact` command's own lines; a fold's
itemized reclaim), they coalesce: one blank separator opens the run, siblings sit
directly under one another with **no interior blank**, and one blank closes it.
The rail connects them into a single quiet aside instead of scattering ticks
through whitespace. No caption `hint` / keybind is rendered unless a real binding
exists (keymap honesty — there is no undo, so compaction shows none).

```
› /compact

┊ Context compacted — 82.6k → 726 tokens
┊ compacted 155 earlier message(s): ~82581 tokens replaced by a
┊ ~726-token summary
┊ Folded 3 spent tool result(s) — reclaimed ~12.4k tokens [B]

```

### 7.7 Working indicator
An **inline** LED-chase readout shown while the agent runs. Never framed, never
a braille spinner, one line:

```
●··· 1:27 ┊ ESC ┊ Responding ┊ ↑177k ↓5.7k ██▊▏··
```

The lit cell bounces across a 4-cell strip. One blank line above/below when
adjacent to other blocks. Telemetry (`↑`/`↓`) and the `ESC` hint are optional.
The trailing 6-cell **flow meter** (§6 motion 5) meters display-stream inflow
on a fixed log scale — bright eighth-block fill, the chase's dim `·` for unlit
cells, a dim `▏` peak tick — and sits last deliberately: end-of-line truncation
drops it first, so the telemetry counters outrank it at narrow widths.

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
families are **EXPLORE / SHELL / EDIT**. Approval is not a family — it is a
lifecycle state a SHELL/EDIT block passes through in place (§8.5). Never invent
another family; never render standalone `READ` / `GREP` / `LS` panels.

### 8.1 Shared block grammar
```
▾ TOOL  meta                                                        ELAPSED
   ┊ <body — rides the `┊` spine, one 2-cell step under the label; unmounts collapsed>
   ┊─────────────────────────────────────────────────────────────
   ◆ DONE  [family extras]             ↑sent ↓recv ┊ cache <n> ┊ ctx <Δ%>
```
**Header** — disclosure `▾`/`▸` (muted) · bold uppercase family label · muted
meta (a path, scope, or the shell command), truncating with `…` · right edge
carries **only the elapsed time** (omitted for a pending `preview`). No state
symbol in the header — the state lives in the footer.

**Spine** — an expanded block reads as ONE unit because a **dim `┊` rail** fills
the label/marker column (col 4, one 2-cell step left of the shared text column)
on every body row: a continuous left edge running from under the header label,
down the body, into the footer hairline and the footer state token. It is the
same soft-rail grammar the reasoning rail and the coalesced notices use — a
**rail, not a frame** (no top edge, no right edge, no box); tool output stays
primary (full-ink content, bold label), reasoning stays recessive (dim). A
**collapsed** block unmounts its body, so the spine shows only when expanded —
exactly when the header and footer are pulled apart and the block would
otherwise float. The rail sits *outside* any diff-row background fill.

**Footer** — the block's last row, always visible, opened by a muted hairline
rule from the body indent to the right rail. Left edge: the **state token** — the
state glyph (`◆ DONE` · `■ ERROR` · `◇ PREVIEW` · `● RUNNING` · `□ CANCELLED` ·
`▲ REVIEW` · `■ DENIED`), colored by state, then the uppercase label. Prominence
is **proportional**: the consequential states — `ERROR`, `DENIED`, `REVIEW` —
keep a **bold** label (news the user must read or act on); the settled-success
and transient states — `DONE`, `RUNNING`, `CANCELLED`, `PREVIEW` — recede, the
colored glyph carrying the state while the label stays **muted, un-bold**, so a
transcript that is mostly successful calls does not shout a column of bold
labels. The glyph is deliberately lossy — `Error` and `Denied` share `■` — and
the **label carries the distinction the shape cannot**. After it, `┊`-joined
family extras (EDIT counts + note, SHELL `EXIT <code>` + result meta, or an
in-review block's danger-toned reason + awaiting-decision note / approval note). Right-bound:
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
expanded it), and a **pending preview / review** (`◇ PREVIEW`, `REVIEW`)
arrives expanded so its body can be inspected before deciding (it collapses
once applied/settled). `ctrl+o` toggles **all** foldable
blocks at once — tool blocks and thinking rails: if any is collapsed it
expands them all, otherwise it collapses them all. A **click on a block's
header row** toggles that one block. State is per-block; an explicit user
expand/collapse survives the block's in-place rebuilds.

`/find` searches canonical transcript content — the body of a collapsed block
is searched even though it is unmounted from the view. Jumping to a match
inside a collapsed block expands it; the newest match stays clear of the find
indicator row.

**Preview budget (breathes with height).** A running block's bounded live tail
(and any streamed error/cancel tail) previews at most **`clamp(pane_height / 5,
8, 24)`** physical rows — at height/5 the preview never claims more than a fifth
of the viewport, so a tool block cannot dominate the pane. The **floor is the
historical fixed 8**, so a pane ≤ 40 rows is byte-identical to before; a taller
pane lets the tail breathe up to the ceiling of 24. This is a **print-time**
decision measured against the last-known terminal height: a block printed before
a resize keeps its printed preview size (rows are immutable in scrollback); only
the next block built uses the new height. The elision affordance (`… N earlier
lines hidden`) and the stored full output are unchanged — only the budget moves.

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

### 8.5 Approval — the gated block's own review lifecycle
Approval is **not a family** and never a separate panel or docked box. A gated
call is reviewed **inside its own tool block** (SHELL or EDIT): the block's
footer **state label walks the lifecycle** —
`REVIEW → RUNNING → DONE`/`ERROR` when approved, or `REVIEW → DENIED` when
refused. One block, start to finish; the tool's command/diff is never
duplicated in a second block.

- **`REVIEW`** (orange, no elapsed) **arrives expanded** — you must see what
  you authorize. The body is the block's own body: the `$ command` (SHELL) or
  the **diff** (EDIT). The footer carries, in order: an optional **danger-toned
  reason** (`destructive` · `N pre-existing changes` · `unsandboxed`) in the
  danger role, then a dim **`awaiting decision`** note. The block only
  *signals* — the decision keymap (`y approve ┊ n deny` plus `a always` /
  `p project` **only when the loop offers them**) renders exactly once, at the
  composer (§8.5): one affordance, one place, where the keys are pressed.
- **Manual approval** folds a muted **note** into that same footer (`approved
  this time` / `approved this session` / `approved this project`) and drops the
  affordance in place; the block then flips to `RUNNING` when it starts, and the
  note rides through to `DONE`.
- **Auto-approval carries no chrome** — the tool block alone is the record.
- **EDIT** review reuses the preview block: `◇ PREVIEW → REVIEW` flips **in
  place** (the diff IS the review surface), then `RUNNING → DONE`, or `DENIED`.
- **`DENIED`** (red, no elapsed) is terminal: the tool never ran, so the block
  is the honest record of what was proposed and declined.
- While the decision is pending, the **bottom statusline** and the **composer
  frame + placeholder** take the REVIEW posture (§9.2/§9.3): the same
  `▲ REVIEW` symbol+label, and the placeholder carries the full offered keymap
  (`y approve ┊ n deny ┊ …`) — the affordance's one home, at the eye's resting
  place, so the decision is never lost off-screen and never printed twice.
  Those cues carry no new state; they key on the same `awaiting_approval` flag
  and revert with the block.

### 8.6 Diff rendering (`DiffBlock`) — shared by EDIT & the in-block review
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
  `◉ open` for the composer-referenced file. A **collapsed** directory carries
  the §9.1 state cluster as a rollup (`±N ◇M`) over the files beneath it, with
  the file count as the muted tail; the count drops before the state at width.
  `↵` on a file inserts `@<relative-path>` into the composer; `/` filters flat
  (parent path as dim meta). Data: `git ls-files --cached --others
  --exclude-standard`, plain readdir outside a repo; 500 visible rows, then a
  dim `… N more` row.
- These are **disclosures, not sidebars**: invariant #1 stands — nothing
  persistent, nothing beside the transcript.

#### 9.1.2 The job card (the pinned governing prompt)

When the newest user prompt has scrolled above the viewport, its text is pinned
as a quiet **band** directly under the session bar — the machine's **job card**,
the governing instruction for everything on screen below it, so the reader always
knows which prompt the visible content answers (grok `sticky_headers`). Pager
mode only; there is no inline-mode band. It reads as an extension of the top
chrome — **not** a card floating in the transcript.

```
~/iris-agent ┊ git main                      CTX 94k/300k ●●●○○○○○○○
────────────────────────────────────────────────────────────────────  ← session bar hairline

  ▸ › Overhaul the settings menu. First, prune the settings and…   +4
────────────────────────────────────────────────────────────────────  ← band hairline (SAME rule)
```

- **Same columns as the transcript.** The `›` marker sits on the user column
  (col 4) and the body hangs at col 6 — a prompt looks identical whether pinned
  or scrolled into view (§7.1). Continuation lines hang unmarked at col 6.
- **Ink text in muted chrome.** The prompt's text renders in body ink
  (`panel_style`) — the one piece of legible content in the top chrome, readable
  at a glance. The chrome around it stays muted: the `▸`/`▾` disclosure and the
  `›` marker are muted **bold**, the closing rule is muted. Not orange, no fill;
  the surrounding tones still read the band as chrome, not the live turn.
- **Honest when collapsed.** Collapsed, the band is one row; when wrapped rows
  are hidden it ends in a right-aligned dim `+N` (the house `+N more` idiom,
  shortened — the band has no room for prose). No marker when nothing is hidden.
- **Toggle.** A click on the band row, or the key `o` while the scrollback list
  holds focus in pager mode (the list-state law, §9.1.1), expands it to the full
  wrapped prompt and collapses it again. **ctrl+o never routes here** — that is
  fold-toggling's one meaning everywhere (§8.1). Expansion resets to collapsed on
  each new user message.
- **Closed by the session bar's own hairline.** The band's bottom rule is the
  **same** inset dim `─` the session bar draws (col 2 → width−2), byte-for-byte —
  never the composer's full-width border weight. Two matching hairlines bracket
  the band as one top-chrome region.
- Yields the top row to an interactive overlay (a selection or search match
  revealed exactly at the viewport top keeps its highlight).

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

The frame is also the machine's **bezel lamp**: both weights render in their
`border`/dim tones at rest and take the **orange accent** while a review waits
(§8.5) — reinforcement for the `▲ REVIEW` readout, never the sole signal (the
text carries state; the monochrome test still passes). The empty-buffer
placeholder likewise becomes a dim decision echo for the duration (§8.5).

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
| **REVIEW posture** (`awaiting_approval`) | leading segment is `▲ REVIEW` (orange symbol, bold label); every other segment dims |

**Narrow widths, drop in order:** policy → effort → minimum `◉ CODE ─ MODEL`.
cwd/branch/context NEVER appear here — they live on the session bar.

**The REVIEW posture (§8.5).** While a gated tool awaits the user's decision
(`awaiting_approval`), the leading segment swaps `◉ MODE` for `▲ REVIEW`
(`symbols::REVIEW`, orange, bold label — the same symbol+label the gated
block's footer shows, echoed at the eye's resting place) and **every other
segment dims**: the model button drops its underline (it is not clickable while
the composer is frozen), and effort and the policy symbol lose their tone, so
the line has one lit subject. `▲ REVIEW` inherits `◉ MODE`'s never-dropped
slot, so the narrow-width minimum becomes `▲ REVIEW ─ MODEL`. The swap is a
static state readout — ticks stay stopped during the wait (no flash) — and it
reverts to the exact prior rendering on approve/deny/cancel.

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

### 9.7 The exit receipt

When a session that ran at least one turn ends, Iris prints **one dim line**
after terminal teardown — the instrument's printed slip, landing in normal
terminal scrollback in both screen modes (in pager mode it is the only trace
of the run; inline it closes the transcript):

```
iris 0.1.0 ┊ 12m ┊ 3 turns ┊ ↑412k ↓18.9k ┊ cache 88%
```

Fields, in order, `┊`-joined under the separator law: product + rev · wall
time · turn count · tokens sent/received summed over **every provider turn**
(the billing measure — unlike the per-task divider) · the cached share of
sent tokens. **Numbers are honest** (§11): a field the runtime did not
measure is omitted, never guessed; a session with no turns prints nothing —
a receipt for nothing is noise.

---

## 10 · Overlays

Overlays are **docked, frameless menus** above the composer — the same grammar
as the tool blocks (§8) and the start-page launcher (§12.5), never a bordered
dialog. There is **no box-drawing frame, no shadow, no scrim.** Structure comes
from three parts, built by one shared renderer (`overlay_menu`):

- a **bold uppercase title** header (omitted for the title-less SlashMenu);
- **rows** whose highlight is the `surface` fill across the menu measure — never
  a border, never a colored accent — with a bold label; `◉`/`○` mark a
  current/enabled row (never `[x]`);
- an optional **dim key-hint footer**, set off by one blank row.

The composer's top edge sits directly below, so the menu needs no frame of its
own to read as a distinct region.

- **SlashMenu** — command palette (§9.5). Title-less: just the rows.
- **Picker** — **tasks** and resume. Rows: `[◉ if active] label … meta hint`.
  The model switcher, scoped-models, providers, and project-permissions surfaces
  are **no longer pickers** — they are hatches inside the faceplate (§10.1).
  **Adjacent things share one picker** stands, now enforced *structurally*: the
  model hatch's own `reasoning` row IS the effort track (§10.1), so there is no
  second, duplicated track. `/model` and a bare `/reasoning` open the faceplate's
  ENGINE hatch; the typed forms (`/model <id>`, `/reasoning <level>`) stay the
  fast path. Never a second bespoke list for a sibling of an existing surface.
- **Settings panel** — the faceplate (§10.1). Not a category tree. Its ports are
  **hatches, not doors**: they expand in place, never swapping to another modal.
- **HelpOverlay** — the `?` cheatsheet: grouped key→action rows (keys in ink,
  actions muted, quiet uppercase group headings). No color, no icons.

### 10.1 The settings panel — the faceplate

`/settings` is ONE flat control surface, like the printed back panel of a lab
instrument: every setting is a row, grouped under dim uppercase **silkscreen
section headers** (`ENGINE · SAFETY · MEMORY · CHECKS · PANEL · GIT` — what
runs → what it may do → what it remembers → how it self-checks → the panel
itself → where it works), and adjusted **in place**. No sub-menu is ever
opened to change a value; drilling three levels to flip a switch is the
anti-instrument.

```
SETTINGS                                                    iris 0.1.0

ENGINE
  model             ▸ gpt-5.5 ┊ openai-codex
  reasoning         ○ off  ○ minimal  ○ low  ◉ medium  ○ high  ○ xhigh
  model scope       ▸ all enabled
  providers         ▸ 3 connected

MEMORY
  compact at        ●●●●●●○○○○  232k tokens
  microcompaction   ○ off  ◉ on
  watermark         ●●●●●○○○○○  32k tokens

↑↓ select · ←→ set · esc close
```

Pressing `↵` on a `▸` port **expands it in place** — the marker flips to `▾`
and the surface's rows print indented directly beneath, inside the same panel.
The model hatch open, its `reasoning` row live-tracking the highlighted
candidate:

```
ENGINE
  model             ▾ gpt-5.5 ┊ openai-codex
    ◉ gpt-5.5                          openai-codex   default
    ○ claude-sonnet-5                  anthropic
    ○ gemini-3-pro                     google
  reasoning         ○ off  ○ minimal  ○ low  ◉ medium  ○ high  ○ xhigh
  model scope       ▸ all enabled

←→ reasoning · ↵ set default · s session · esc collapse
```

**Masthead.** Row one is the panel's silkscreen: bold `SETTINGS`, the crate
rev right-bound on the panel measure (the same identity print as the start
page and the exit receipt). It is pinned — a windowed panel scrolls its
sections under it, never past it.

**Four control archetypes** — a closed set, like the four tool families.
Never invent a fifth:

- **switch** — a fixed vocabulary printed as a labeled detent track
  (`○ strict  ◉ auto  ○ never`). `←`/`→` click one detent and **clamp at the
  stops** (a real switch never wraps; against the stop is a silent no-op).
  Bools are two-position switches (`○ off  ◉ on`). The `◉` is the handle —
  orange wherever it sits (selection color, not state color); the one guarded
  switch (`skip approvals`) paints its handle **danger red in the on
  position** and carries a permanent dim caution silkscreen
  (`dangerous ┊ session only`). When the labeled track does not fit the
  width, the row degrades to its **rotary form** — position dots + the
  selected value (`○○◉○○  medium`) — width alone decides, per row.
- **dial** — a numeric on a **10-detent ladder** rendered as the house 10-dot
  meter (filled `●`, orange edge, dim `○`) plus the honest printed value
  (`232k tokens` — the ONE house token format). `←`/`→` step to the
  neighbouring detent; an off-ladder value (hand-edited json) snaps into the
  ladder on its first click while the printed number stays true. `↵` opens an
  inline register for a precise value, clamped to the field's hard bounds.
- **register** — free text edited inline on the row: `↵` edits (buffer + the
  `▋` caret), `↵` saves, `esc` cancels, an empty buffer clears the key when
  the field allows it; a rejected buffer shows an inline danger token
  (`■ whole numbers only`), never a modal.
- **port** — a `▸` row that is a **hatch, not a door**: `↵` expands it in place
  to `▾` + indented child rows inside the same panel (model picker, model scope,
  providers, project permissions). **One hatch open at a time** (accordion —
  expanding one folds any other in the same keypress); `↵` on the `▾` header or
  `esc` anywhere folds it (cursor lands back on the header); `←`/`→` are never
  collapse verbs. The panel never leaves — no surface replacement, no frame
  without the faceplate. Child rows print at a four-cell indent and degrade like
  their archetypes at narrow widths; the footer is contextual to the selected
  child (its true verbs). The **model row is a rotary–port hybrid**: `←`/`→`
  cycles the scoped models exactly like Ctrl+P (the row rebuilds on the new
  engine and flashes), `↵` expands the hatch; its footer names both verbs
  (`←→ cycle · ↵ open`). The collapsed value prints the **active session
  engine** (not the persisted default); a session-only `s` pick that diverges
  from the default carries a quiet `· session` tag so the row never lies about
  what is running. Inside the model hatch the panel's own `reasoning` row
  IS the effort track — arrowing over candidates re-renders it with that model's
  levels, target clamped, and there is no duplicated second track.
  **Dialog-guard exception:** three genuine interrupts (the large-context switch
  advisory, the OAuth login dialog, the API-key dialog) still overlay the
  faceplate; when one resolves — any path — the panel's snapshot is refreshed (a
  login can grow the catalog) and it reopens expanded with the cursor intact,
  *before the next draw*, so the dock never collapses for a frame.

**Mechanics.** `↑`/`↓` move over controls (wrapping; headers and blanks are
skipped — silkscreen is not selectable). Every adjustment **saves
immediately** (position IS state, like a physical switch) and the changed
element renders bright for two ticks — the §6 detent flash, on the same tick
grid as the statusline detents, settled instantly under reduced motion. The
theme row is a **live rotary**: each click re-skins the whole pane before
your eyes. A **dependent control dims to inert hardware** while its master is
off (the watermark under `microcompaction ○ off`) but stays operable. The
footer prints only the selected row's true verbs (`←→ set` · `←→ adjust · ↵
type` · `↵ edit` · `↵ open` — keymap honesty per archetype).

**Height honesty.** On a tall pane the whole faceplate prints at once. On a
short one the panel windows itself under the pinned session bar and above the
protected composer, scrolling with the house `(n/N)` position row — never
clipped, never painted under other chrome. The design floor is a 12-row
terminal: panels window all the way down to it; below that floor the footer
may clip.

**Pruning.** The faceplate is curated; the service hatch is `settings.json`.
Niche flags (bash tool mode, tool round-trip caps, retry tuning, custom
endpoint blocks) stay json-only. Every panel row must earn its silkscreen.

---

## 11 · Casing & content

- **Sentence case** for all prose.
- **UPPERCASE** is reserved for structural labels: tool families
  (`SHELL`/`EXPLORE`/`EDIT`), states
  (`DONE`/`RUNNING`/`ERROR`/`REVIEW`/`DENIED`/…), mode (`CODE`), section labels
  (`PLAN`/`THINKING`), and `EXIT`. **Never** uppercase for emphasis in prose.
- **Numbers are honest.** Token telemetry (`↑177k ↓5.7k`), durations (`7.6s`,
  `1:27`), counts — shown compactly and only when real. Never assert savings the
  runtime hasn't measured.
- **Brevity.** Hints are short and inline (`↵ to send · shift+↵ for new line · /
  for commands`), `·`-joined — `•` stays the markdown bullet's alone (§5). At a
  narrow width a hint row drops whole trailing fields, never clipping one
  mid-word: a printed control either fits or is omitted. Placeholders use exact product casing.
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

The launcher **is the home menu**: `New session` · `Resume session` · `Tasks` ·
`Settings` · `Quit`, each a keyboard row (`↑`/`↓` + `↵`, or the `ctrl-` chord).
`Tasks` opens the unified task surface (`/tasks`, §10 Picker). Recoverable Iris
tasks (ADR-0031) are surfaced **here** — a dim `· N to recover` badge on the
`Tasks` row — never a picker forced open over the menu on launch.

```
~/demo ┊ git main                                     CTX 0/300k ○○○○○○○○○○

                        ○ ○ ○ ● ○ ○ ○ ○ ○ ○ ○ ○        ← IrisMark (animated)
                        I R I S                 0.1.0  ← silkscreen (printed)

                        ◉ New session ················ ctrl-n
                          Resume session ·············· ctrl-r
                          Tasks · 2 to recover ········ ctrl-t
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

**Silkscreen.** One row directly under the strip — printed faceplate text, so
it is visible from the first frame and never animates: the letter-spaced
wordmark `I R I S` anchored to the strip's **left** edge, the crate rev
anchored to its **right** edge (dim). Wordmark in body ink, plain weight — the
LEDs stay the only bright element. This is the interface's one version
surface and its only wordmark; still no ASCII art, no figlet.

**Power-on.** An interactive launch runs the **lamp test** (§6 motion 3):
frame 0 shows the silkscreen printed, the strip dark, and the menu hidden
(blank rows — the block's height never changes, so nothing reflows); the
LEDs then fill left-to-right two per tick, hold all-lit for two ticks —
every LED proves itself — and release into the idle ping-pong as the menu
rows go live. Any key completes the boot instantly and still performs its
normal action; the composer is live throughout; under reduced motion the
page starts settled. The boot exists only here: launching with a task or a
resume target powers straight into work, no ceremony.

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
   every body (prose, tool, reasoning) hangs on ONE text column, and every
   right-aligned readout (elapsed, telemetry, diagnostics) aligns to ONE right
   rail. Indentation is hierarchy, stepped in 2-cell units (gutter · label ·
   body, §4) — never an ad-hoc indent.
4. **Block rows** are each exactly one of {header·body·footer rule·footer} and
   all share one width; no row overflows the block's rails.
   4a. **One marked voice.** The transcript marks the user's turn with a `›` in
   the gutter and nothing else; the agent speaks unmarked (§7.1–7.2).
5. **Three tool families only** (EXPLORE / SHELL / EDIT). No standalone
   READ/GREP/LS/DIFF panels; approval is an in-block lifecycle state, never a
   separate panel.
6. **Chrome is for tools.** Conversation, thinking, plans, and notices are never
   boxed. Boxes are never used for prose. **Overlays are frameless too** — menus,
   pickers, and the slash palette carry no box-drawing frame; selection is the
   `surface` fill (§10).
7. **Square corners always** (`--radius: 0`).
8. **State = symbol + label + color**, never color alone; the pane passes the
   monochrome test.
9. **One type size.** Hierarchy never uses a larger font in the pane.
10. **Closed symbol set.** No glyph outside §5; `…`/`−`/`┊` (not `...`/`-`/`|`);
    no emoji.
11. **Composer is unconditional.** No show/hide/reveal/collapse mechanic.
12. **Motion** is only the closed quantized set of §6 — LED chase (working
    indicator + IrisMark), edge pulse, the start page's one-shot lamp test,
    and the two-tick detent flash — all stepped on the tick grid, all
    reduced-motion safe, and none of them ambient.

---

## 14 · Anti-patterns (do NOT)

- ✗ A role card / bubble / avatar for user or assistant messages.
- ✗ Marking the **agent** with a `›` (it decorates the dominant voice); mark the user's turn instead (§7.1).
- ✗ An ad-hoc indent that doesn't land on the gutter/label/body ladder, or a right-aligned readout inset differently from the tool elapsed (§4).
- ✗ A colored left-border accent on active rows (use the `surface` fill).
- ✗ Boxing a code block, a plan, a notice, or tool output — nothing in the transcript is boxed.
- ✗ A braille spinner, a rainbow/percentage meter, or an animated progress bar.
- ✗ A larger font, all-caps prose, or bold-for-emphasis to signal importance.
- ✗ Emoji, gradients, rounded corners, drop shadows in the transcript, glass/blur.
- ✗ ASCII `|` separators, ASCII `-` removals, or `...` ellipses.
- ✗ Asserting efficiency/savings the runtime has not measured.
- ✗ A fifth tool family, or a standalone READ/GREP/LS/DIFF panel.
