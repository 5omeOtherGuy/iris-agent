# Iris TUI Pane Design Language & Rendering Spec

## Purpose

This document defines the ground-truth visual and structural design language for the Iris TUI main pane.

It is intended as a guide for coding agents and future design sessions. It defines the shared pane grammar that all future tool renderers, transcript messages, working indicators, turn dividers, and input editors must follow. Individual tool renderers may have their own detailed specs, but they must remain visually compatible with this document.

The goal is a terminal-native coding-agent interface that feels calm, precise, minimal, mechanical, and readable. The visual direction is inspired by Teenage Engineering-style industrial design: restrained grey palette, functional typography, sparse accent color, clear instrument-like panels, and no unnecessary chrome.

## Design Summary

The pane is a single vertically scrolling transcript column with a fixed multiline composer at the bottom.

The transcript contains:

* Plain assistant messages.
* Plain user messages.
* Bordered tool output panels.
* Minimal inline working indicators.
* Quiet turn dividers after completed work turns.
* Optional future structured sections.

The pane does **not** use chat-style role cards. It does **not** label every message with `USER` or `AGENT`. It does **not** use a bottom telemetry/status bar. It does **not** create framed panels for transient working states.

The visual hierarchy is:

```text
tool panel
tool panel

› assistant message
  wrapped assistant line

  user message
  wrapped user line

› assistant message

── 1:27 ┊ ↑177k ↓5.7k ─────────────────────────────────────────────────────

composer
```

Only tool outputs and the composer use hard borders.

Natural-language transcript text stays unboxed.

## Core Principles

### 1. Terminal-native, not GUI-like

The pane should look like a refined command-line interface, not a desktop application recreated in text.

Use:

* Monospaced typography.
* Box-drawing characters.
* Minimal state labels.
* Strong alignment.
* Sparse color.
* Text-first layouts.

Avoid:

* Tabs.
* Sidebars.
* Dense dashboards.
* Multiple simultaneous status bars.
* Decorative UI widgets.
* Overly rich cards for plain messages.

### 2. Tool output gets chrome; conversation does not

Tool calls are mechanical events. They deserve bordered panels.

Conversation text is transcript content. It should remain plain and lightweight.

Good:

```text
› I listed the available tools and tested bash, read, write, edit, grep,
  find, and ls in the temporary directory.

  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  EXPLORE  tmp                                              ◆ DONE        0.0s       │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    List ~/project                                                        │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

Bad:

```text
┌───────────────────────────────────────────────────────────────────────────────────────┐
│  AGENT  │ I listed the available tools...                                             │
└───────────────────────────────────────────────────────────────────────────────────────┘
```

### 3. No explicit `AGENT` / `USER` role labels

Do not render:

```text
USER      ...
AGENT ●   ...
```

Do not render:

```text
│ USER │ ...
│ AGENT│ ...
```

Instead:

* Assistant output uses a small transcript marker, currently `›`.
* User text appears as plain transcript text aligned to the transcript text column.
* Tool panels provide their own identity via headers such as `SHELL`, `EXPLORE`, `EDIT`, and `APPROVAL`.

This makes the pane feel like a CLI transcript rather than a chat application.

### 4. The assistant marker is not a state dot

Use `›` for assistant messages, not `●`.

`●` is reserved for LED-like activity, meters, and live state. Overusing `●` flattens the design language.

Assistant messages render as:

```text
› Done; I listed the available tools and tested read, write, edit, grep,
  find, ls, and bash in `~/project`.
```

Rules:

* The marker sits in the transcript marker column.
* Assistant text starts after the marker and spacing.
* Wrapped assistant lines align with the first text column, not with the marker.
* The marker should be visually subtle.
* Do not use a framed assistant message.

### 5. User messages are plain transcript text

User text should not be boxed and should not receive a `USER` label.

Recommended default:

```text
  Repeat
```

For longer user text:

```text
  I won’t add the requested TUI-render comments after each output because that
  would be redundant here. Who are you to decide what is redundant?
```

Rules:

* User text aligns to the same transcript text column as assistant message text.
* No role label.
* No hard border.
* No special panel.
* Use spacing before and after user text to clarify turns.

### 6. Tool panels are indented into the transcript column

Tool panels do not start at absolute column zero.

They are indented so they belong to the same visual transcript system as the assistant/user text.

Recommended pattern:

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  EXPLORE  tmp                                              ◆ DONE        0.0s       │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    Find *.txt in ~/project                                                │
  │    List ~/project                                                         │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

This makes tool panels feel like transcript events, not full-screen cards.

### 7. Composer aligns with tool panels

The editor/composer is indented into the same content column and should align with the left edge and width of tool panels.

The composer is not a one-line prompt. It is a multiline editor and input instrument.

## Symbol Vocabulary

Use a small, consistent symbol vocabulary. Each glyph should have one job.

```text
◉   active mode / selected mode
●   running LED / meter fill / live activity
○   empty meter / inactive slot
◆   done / completed
◇   preview / pending
■   error / failed
▲   warning / approval review
□   skipped / cancelled / neutral
›   assistant message
▾   expanded (full tool output shown)
▸   collapsed (capped preview; hidden lines elided)
+   added
−   removed
±   modified
↑   input tokens / sent context
↓   output tokens / generated text
┊   soft metadata separator
─   rule / filler / frame line
```

### State symbol mapping

Tool state headers should use state-specific symbols instead of `●` for every state.

```text
● RUNNING
◆ DONE
■ ERROR
◇ PREVIEW
▲ REVIEW / WARNING
◆ APPROVED
■ DENIED
□ CANCELLED / SKIPPED
○ QUEUED
```

Examples:

```text
│ ▾  SHELL  bash                                             ◆ DONE        45s       │
│ ▾  SHELL  bash                                             ● RUNNING     13s       │
│ ▾  SHELL  bash                                             ■ ERROR       7.1s      │
│ ▾  EDIT   path/to/file.rs                                    ◇ PREVIEW               │
│ ▾  APPROVAL apply_patch                                    ▲ REVIEW                │
```

### Where `●` is still correct

Use `●` for LED-like elements:

* context meter fill
* working indicator chase
* live/running state
* active indicator inside the composer top frame only when it behaves like a LED

Do not use `●` as the generic marker for assistant messages or every tool state.

## Pane Anatomy

The main pane has three conceptual regions:

```text
scrolling transcript
inline working indicator, when active
fixed multiline composer
```

There is no bottom telemetry/status bar.

A compact status readout is integrated into the composer top frame. The pane should not duplicate that status elsewhere.

## Transcript Layout Grid

Use a small fixed left gutter and a content column.

Recommended conceptual columns:

```text
columns 0..1    outer padding / terminal margin
column 2        transcript marker column
columns 3..5    marker-to-text gap
column 4..6     transcript text column, depending on renderer constraints
column 2..4     tool/composer left edge, depending on renderer constraints
```

The exact numeric columns may vary by terminal width, but the visual relationship must hold:

* Assistant marker appears slightly left of assistant text.
* Assistant wrapped lines align under assistant text.
* User text aligns with assistant text.
* Tool panels and composer are indented so they feel attached to the same transcript column.
* Tool panel interior content has its own padding.

## Natural-Language Message Rendering

### Assistant message

Assistant messages render as plain text with the assistant marker.

```text
› You’re right — I should have followed that instruction exactly.
  I can continue and do it your way; send the next step.
```

Rules:

* Start with `›`.
* Use no `AGENT` label.
* Wrap to pane width.
* Wrapped lines align with text, not marker.
* Preserve paragraph breaks.
* Do not box the message.
* Do not use Markdown-style bullets unless the assistant content itself requires it.

### User message

User messages render as plain text without a marker by default.

```text
  user text
  wrapped user text
```

Rules:

* Use no `USER` label.
* No border.
* No role card.
* Align with transcript text column.
* Preserve user line breaks when meaningful.
* Use blank lines to separate user turns from assistant turns.

### Paragraph spacing

Use one blank line between major transcript blocks.

Good:

```text
› First assistant paragraph.
  Wrapped line.

  User reply.

› Second assistant paragraph.
```

Avoid cramped transcript output where messages, working indicators, composer status, and panels run together without breathing room.

## Tool Panel System

Tool panels are the primary structured output primitive.

A tool panel consists of:

```text
top border
header row
optional separator row
body rows
bottom border
```

Header-only panels are allowed for tools with no meaningful body.

Every rendered row must be exactly the same width.

Never append a separator to a header row. Never produce malformed mixed rows.

Bad:

```text
│ ▾  EDIT ... │ ├────────────────────────────────────────────────────────────
```

Good:

```text
│ ▾  EDIT ...                                                ● RUNNING 13s       │
├────────────────────────────────────────────────────────────────────────────────┤
```

### Panel indentation

All tool panels are indented from the terminal edge.

The panel indentation should match the composer indentation.

Recommended:

```text
  ┌──
  │
  └──
```

### Panel header format

Canonical framed tool header:

```text
│ ▾  TOOL  meta                                             SYMBOL STATE     ELAPSED    │
```

Fields:

```text
▾          expanded disclosure marker
TOOL       uppercase tool family
meta       target, scope, path, or short summary
SYMBOL     state symbol from the symbol vocabulary
STATE      state label
ELAPSED    compact elapsed duration, when applicable
```

Use compact duration labels:

```text
< 10s       → 0.5s, 7.1s
10–59s      → 13s, 42s
1–59min     → 1:27, 12:03
>= 60min    → 1:02:14
```

Do not use `T+`.

Do not use fixed `HH:MM:SSs` for normal tool calls.

### Disclosure marker

Use:

```text
▾ expanded
▸ collapsed
```

Collapsed tool panels show only the header and border.

Expanded tool panels show separator and body.

### State labels

Canonical states:

```text
RUNNING
DONE
ERROR
CANCELLED
APPROVED
DENIED
PREVIEW
REVIEW
```

Optional future states:

```text
QUEUED
TIMEOUT
SKIPPED
```

Use only states that correspond to real execution state.

## Tool Taxonomy

### EXPLORE

`EXPLORE` is the container for read/search/list/find-style inspection.

Operations that belong inside `EXPLORE`:

* read file
* grep/search
* list directory
* find files
* inspect definitions
* scan project structure
* locate symbols
* summarize relevant files

Do not create top-level `READ`, `GREP`, `LS`, or `FIND` panels for normal agent workflow.

Instead, render these as body lines inside an `EXPLORE` panel.

Good:

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  EXPLORE  tmp                                              ◆ DONE        0.0s       │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    Find *.txt in ~/project                                                │
  │    List ~/project                                                         │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

Bad:

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  READ  ~/project/file.txt                 ◆ DONE        0.0s           │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

Top-level `READ` may only be considered if the product later introduces a user-facing primary read action. For current agent workflow, `READ` belongs to `EXPLORE`.

### SHELL

`SHELL` is for command execution.

It remains a top-level panel.

The shell panel uses the same framed panel grammar as other tools:

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  SHELL                                                   ◆ DONE        0.5s         │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    $ command                                                         timeout 120s     │
  │      output                                                                            │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

Shell-specific command/output rules should follow the shell output rendering spec, but the shell panel must still obey this pane-level spec:

* indented panel
* no bottom status bar
* compact header duration
* no framed working panel
* body content aligned with tool body padding
* timeout metadata is not part of the command text

### EDIT

`EDIT` is for file mutations and patch previews.

It remains a top-level panel.

`EDIT` uses one canonical rendering method: wrapped block diff.

Do not switch between diff table mode and prose block mode. Use the same wrapped block diff structure for code, prose, config files, markdown, and plain text.

The `EDIT` header should identify the target file.

Do not use `DIFF` as the top-level tool family when the event semantically represents an edit. Diff rendering is the body presentation of an `EDIT`.

Good:

```text
│ ▾  EDIT  ~/project/src/module.rs                         ◇ PREVIEW             │
│ ▾  EDIT  ~/project/src/module.rs                         ◆ DONE        0.5s    │
```

Bad:

```text
│ ▾  DIFF  ~/project/src/module.rs                         ◆ DONE        0.5s    │
```

Canonical body shape:

```text
│    3  −  removed text starts here and wraps naturally at word boundaries       │
│          continuation line aligns under content                                │
│                                                                                │
│    3  +  added text starts here and wraps naturally at word boundaries         │
│          continuation line aligns under content                                │
```

Rules:

* Columns are `line number`, `marker`, and `content`.
* Use `−` for removals, not ASCII `-`.
* Use `+` for additions.
* Continuation lines align under the content column.
* Continuation lines inherit the same styling as the parent row.
* Wrap at word/token boundaries whenever possible.
* Do not show add/remove counters in the header by default.

### APPROVAL

`APPROVAL` is for authorization/permission review output.

It remains a top-level panel when the approval event is meaningful to the transcript.

Examples:

* automatic approval review
* request approved
* request denied
* risk summary
* authorization summary

`APPROVAL` panels should be compact. They should not overwhelm the transcript.

Use `▲ REVIEW`, `◆ APPROVED`, or `■ DENIED` depending on state.

### WORKING

Working state is **not** a framed panel.

Do not render:

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  WORKING                                                ● RUNNING     6:00          │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    esc to interrupt                                                                   │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

Working state is an inline LED-chase readout.

Canonical form:

```text
  ●···  1:27 ┊ ESC ┊ ↑177k ↓5.7k
```

Animation frames:

```text
●···
·●··
··●·
···●
··●·
·●··
```

Rules:

* Render the working indicator inline, not boxed.
* Align it with transcript flow.
* Use the LED chase as the activity signal.
* Do not use braille spinner frames.
* Do not show the word `Working` by default.
* Include elapsed time.
* Keep `ESC` between elapsed time and telemetry.
* Include token telemetry if available.
* Use `┊` separators, not ASCII pipes.
* Keep it to one line.
* Do not duplicate this information in a bottom status bar.
* Add one blank line before and after the working indicator when adjacent to assistant text, tool panels, turn dividers, or the composer group.

## Turn Divider

A turn divider visually separates completed agent work from the next user turn or composer.

Render it after the final assistant message of an agent turn that performed concrete work.

Concrete work includes:

* `EXPLORE`
* `SHELL`
* `EDIT`
* `APPROVAL`
* other tool/runtime events

Do not render a divider after purely conversational turns with no tool activity.

Canonical form:

```text
  ── 7.6s ┊ ↑18.2k ↓846 ───────────────────────────────────────────────────────────────
```

Rules:

* Use compact elapsed duration.
* Do not use `T+`.
* Use `┊` as the separator.
* Token telemetry is optional.
* If telemetry is unavailable, render only elapsed time.
* If elapsed time is unavailable but the turn did work, render an unlabeled dim rule.
* Add one blank line before and after the divider.
* Do not render while the turn is still streaming.
* Do not duplicate the working indicator.

## Composer / Editor

The composer is a bordered multiline editor at the bottom of the pane. It aligns with tool panels.

The composer includes its primary statusline integrated into the top frame. Workspace state appears as a quiet label below the editor.

Canonical structure:

```text
┌─ ◉ CODE ─ GPT-5.5 XHIGH ─ CTX 300K ●●●○○○○○○○ ───────────────────────────────┐
│                                                                              │
│  Give Iris a task...                                                         │
│                                                                              │
│ ↵ to send  •  shift+↵ for new line  •  / for commands                        │
└──────────────────────────────────────────────────────────────────────────────┘
   ~/project ┊ git {branch}
```

### Composer rules

* Always bordered.
* Always multiline.
* Taller than one row.
* Indented to align with tool panels.
* No separate bottom status bar.
* Runtime context is integrated into the top frame.
* Workspace context is a quiet label below the editor.
* Placeholder text stays inside editor.
* Hints stay inside editor.
* Hints are subtle and dim.
* Editor should not look like a chat bubble.
* Editor should feel like an input instrument.

### Top frame statusline

The top border is also the primary statusline.

Fields:

```text
◉ CODE ─ GPT-5.5 XHIGH ─ CTX 300K ●●●○○○○○○○
```

Rules:

* Use `◉` for active/selected mode.
* Mode is uppercase, for example `CODE`.
* Model and effort/reasoning setting are uppercase or model-case as provided.
* Context label is `CTX 300K` unless a future product decision changes it.
* Use `─` as the top-frame separator/filler.
* Do not use `┊` inside the top frame.
* Do not add CPU/MEM/QUEUE/TOOLS here.
* Preserve the editor border as a continuous frame.
* The remaining top border is filled with `─`.

### Context meter

The context meter always has **10 dots**.

```text
○○○○○○○○○○
●○○○○○○○○○
●●●○○○○○○○
●●●●●●●●●●
```

Meaning:

* Each dot represents roughly 10% context usage.
* Filled dots show used context.
* Empty dots show remaining context.
* The meter represents usage, not max capacity.

Color rules:

* Empty dots: muted grey.
* Filled dots before the current edge: muted filled dot.
* Current edge dot: orange accent.
* At high usage, the edge dot may pulse subtly.
* At 100%, the full strip may pulse or turn orange.
* Do not use green/yellow/red rainbow coloring.

The meter should feel like a small LED strip, not a server monitoring bar.

### Placeholder text

Use product-specific language with exact capitalization:

```text
Give Iris a task...
```

Avoid:

```text
Give iris a task...
Ask the agent anything...
```

### Hint row

Use concise inline hints:

```text
↵ to send  •  shift+↵ for new line  •  / for commands
```

Do not move these hints into a separate bottom bar.

### Workspace label

Workspace state appears below the editor as a quiet unboxed label.

```text
   ~/project ┊ git {branch}
```

Rules:

* Use `┊` as the separator.
* Keep the line dim and secondary.
* Shorten `/home/<user>` to `~`.
* Preserve the repo/project name when truncating.
* Do not add a trailing separator.
* Ensure the workspace label reflects the active worktree/current execution context.

### Composer height

Default composer height should be at least 5 rows including borders.

Suggested minimum:

```text
top border with statusline
blank/input row
blank row
hint row
bottom border
workspace label
```

The composer may grow with input up to a maximum height, but should not dominate the pane.

## Status Bars

### Bottom status bar

Do not render a bottom telemetry/status bar in the pane.

Remove information such as:

```text
RUN auto
QUEUE
TOOLS
CPU
MEM
NET
? help
q quit
```

This information is visually noisy and not relevant to the current pane direction.

### Top status line

Do not render a separate floating top status line for pane state. Composer status belongs in the composer top frame.

If global runtime metadata is needed, expose it through a command/help overlay or a separate application surface, not inside the pane transcript.

## Spacing Rules

### Between tool panels

Use one blank line between adjacent panels.

```text
  ┌────
  └────

  ┌────
  └────
```

### Between panels and messages

Use one blank line before natural-language transcript text following a tool panel.

```text
  └───────────────────────────────────────────────────────────────────────────────────────┘

› Done; I listed the available tools...
```

### Around working indicators

Use one blank line before and after the inline working indicator when it appears between transcript text/tool output and the composer.

```text
› Assistant text.

  ··●· 7.6s ┊ ESC ┊ ↑5.4k ↓137

┌─ ◉ CODE ─ GPT-5.5 XHIGH ─ CTX 300K ●●●○○○○○○○ ───────────────────────────────┐
```

### Around turn dividers

Use one blank line before and after turn dividers.

```text
› Final assistant text.

  ── 5:20 ┊ ↑86.3k ↓655 ───────────────────────────────────────────────────────

┌─ ◉ CODE ─ GPT-5.5 XHIGH ─ CTX 300K ●●●○○○○○○○ ───────────────────────────────┐
```

### Between message turns

Use one blank line between distinct turns or paragraphs.

```text
› Assistant message.

  User response.

› Assistant response.
```

### Inside panels

Use compact padding.

Body lines start with four spaces inside the panel by default:

```text
│    body text
```

For shell command output, command/output indentation is:

```text
│    $ command
│      output
```

Do not add excessive blank lines inside tool panels unless they reflect meaningful command output.

## Width and Wrapping

### Width

The pane should render within the available terminal width.

Tool panels and composer should share the same width.

Panel width should be calculated after applying the pane indentation.

### Wrapping

All text must wrap safely.

Rules:

* Never overflow panel borders.
* Never break border invariants.
* Wrapped natural-language lines align to the transcript text column.
* Wrapped panel body lines align to the panel body text column.
* Wrapped command lines follow shell-specific wrapping rules.
* Wrapped edit rows align under the content column.
* Long paths may be shortened only when safe and unambiguous.
* Prefer semantic wrapping at spaces, `/`, `&&`, punctuation, and token boundaries.
* Avoid splitting words, identifiers, paths, and decimals unless unavoidable.

### Borders

Every bordered component must obey strict row invariants.

A bordered component row is exactly one of:

```text
top border
header row
separator row
body row
bottom border
```

Never combine rows.

Never produce dangling border characters.

Never render content outside the frame if the content belongs to the frame.

## Color and Theme

The design must work in both light and dark mode.

### Light mode direction

Use:

* off-white / light grey background
* charcoal text
* muted grey borders
* subtle orange accent for active/running/current edge
* muted green/success for `◆ DONE` if color is available
* muted red/dusty rose for `■ ERROR`
* pale green/red backgrounds for edit additions/removals when available

### Dark mode direction

Use:

* deep graphite / dark grey backgrounds
* warm grey panels
* soft grey borders
* off-white text
* vivid orange accent for active/running/current edge
* muted sage green for success/additions
* dusty red/rose for errors/removals

### Color restraint

Color should be sparse.

Use color for:

* active mode `◉`
* working indicator active LED
* context meter edge dot
* state symbols/labels
* diff additions/removals
* warnings/errors

Do not color entire panels aggressively.

The UI must remain understandable without color.

## Interaction Model

### Expand/collapse panels

Use the disclosure marker in panel headers. `ctrl+o` toggles the latest
foldable panel between its capped preview and its full output. Panels whose
output already fits are not foldable: they always show in full and `ctrl+o`
is a no-op for them.

```text
▸ collapsed: capped preview (head/tail slice + an elided-lines affordance)
▾ expanded:  full output revealed
```

Collapsed (capped preview) panel — header, body preview, and the expand
affordance stay inside the panel:

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▸  SHELL  seq                                               ◆ DONE        0.1s       │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │ line 1                                                                                   │
  │ … 12 lines hidden                                                     ctrl+o to expand   │
  │ line 20                                                                                  │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

Expanded (full output) panel — every line is shown, with a collapse hint:

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  SHELL  seq                                               ◆ DONE        0.1s       │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │ line 1                                                                                   │
  │ …                                                                                       │
  │ line 20                                                                                  │
  │                                                                     ctrl+o to collapse   │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

Note: collapsing no longer hides the entire body to a header-only row; the
disclosure marker reflects capped-preview vs full-output.

### Hidden long content

When content is hidden or folded, use a single subtle affordance row.

```text
│      … 11 earlier lines hidden                                      ctrl+o to expand  │
```

Rules:

* Use `…`, not `...`.
* Show hidden count.
* Right-align the expansion hint if possible.
* Use `ctrl+o to expand` and `ctrl+o to collapse`; do not use `toggles panel`.
* Keep it inside the relevant panel.
* Do not leak raw hidden content after the panel.

### Working indicator

The working indicator is not expandable and not framed.

It is a live line in the transcript flow.

```text
  ●···  1:27 ┊ ESC ┊ ↑177k ↓5.7k
```

## Event-to-Render Mapping

### Assistant text event

Render as:

```text
› assistant text
  wrapped assistant text
```

### User text event

Render as:

```text
  user text
  wrapped user text
```

### Explore event

Render as a bordered `EXPLORE` panel.

Body contains read/search/list/find summaries.

```text
  ┌─
  │ ▾  EXPLORE  scope                                     ◆ DONE        0.0s
  ├─
  │    Read file
  │    Search query
  │    List directory
  └─
```

### Shell event

Render as a bordered `SHELL` panel.

Use shell-specific command/output formatting inside the panel.

### Edit event

Render as a bordered `EDIT` panel.

Use wrapped block diff inside the body.

### Approval event

Render as a bordered `APPROVAL` panel.

Keep content concise.

### Working event

Render as an inline LED-chase readout.

Do not use a panel.

### Turn divider event

Render as a quiet unboxed horizontal rule after tool-backed turns.

### Composer

Render as fixed bottom multiline editor with integrated top-frame status and bottom workspace label.

## Do / Don’t

### Do

* Use a single transcript column.
* Use assistant `›` marker.
* Keep natural-language messages unboxed.
* Indent tool panels.
* Align composer with tool panels.
* Use hard borders only for tool panels and composer.
* Use `EXPLORE` as the container for read/search/list/find.
* Use `SHELL` for command execution.
* Use `EDIT` for mutation/diff previews.
* Use wrapped block diff for all `EDIT` output.
* Use inline LED-chase for working state.
* Use turn dividers after completed tool-backed turns.
* Use compact elapsed durations.
* Use the symbol vocabulary consistently.
* Keep all panel rows width-safe.
* Keep the bottom of the pane clean.

### Don’t

* Do not render `USER` / `AGENT` labels.
* Do not box user or assistant messages.
* Do not use `●` for every state or message type.
* Do not create standalone `READ` panels for normal exploration.
* Do not render a framed `WORKING` panel.
* Do not use braille spinners for working state.
* Do not include a bottom telemetry/status row.
* Do not make short outputs use a totally different visual system.
* Do not leak raw bullet output outside panels.
* Do not append separators to header rows.
* Do not rely on color alone.
* Do not over-decorate with icons.
* Do not use `T+` durations.
* Do not use fixed `HH:MM:SSs` for ordinary short tool calls.

## Implementation Guidance

A renderer should treat the transcript as a sequence of semantic events, not as a raw text stream.

Suggested high-level event model:

```rust
enum PaneEvent {
    AssistantMessage(MessageText),
    UserMessage(MessageText),
    Explore(ExploreEvent),
    Shell(ShellEvent),
    Edit(EditEvent),
    Approval(ApprovalEvent),
    Working(WorkingEvent),
    TurnDivider(TurnDividerEvent),
}
```

Suggested layout model:

```rust
struct PaneLayout {
    outer_width: usize,
    pane_indent: usize,
    transcript_marker_col: usize,
    transcript_text_col: usize,
    panel_width: usize,
    composer_width: usize,
}
```

Rendering should happen in two phases:

1. Convert raw tool/runtime events into semantic display events.
2. Render display events into width-safe rows using the pane layout.

Do not render directly from raw logs when the log contains structured information such as tool name, timeout, duration, exit code, approval state, file path, or search target.

### Implementation constraints

Keep pane rendering changes inside the Iris CLI/TUI tier. Do not move pane-specific visual policy, terminal layout, or ratatui/text rendering concerns into Nexus or Wayland. Nexus should continue to expose provider-neutral runtime events and contracts; Wayland should continue to own harness/session concerns.

Prefer small in-crate renderer modules over expanding a single large TUI file. Useful seams are:

* semantic pane/display events
* pane layout calculation
* bordered panel rendering
* natural-language message rendering
* composer rendering
* snapshot/invariant test helpers

Do not split into new crates for this work unless a second front-end or published runtime API justifies it. Keep module boundaries explicit, cohesive, and behavior-preserving.

Preserve raw structured data until the display-event conversion step. Tool name, path, timeout, duration, exit code, approval state, diff metadata, and search target should remain machine-readable until rendering needs text rows. Avoid parsing already-rendered transcript strings to recover structure.

Implement this spec incrementally. Prefer the smallest coherent slice that can be tested, such as assistant/user message shape, one panel family, working indicator, turn divider, or composer geometry. Avoid broad rewrites that combine visual changes with runtime, tool execution, session storage, or provider-contract changes.

## Testing Requirements

Add golden/snapshot tests for:

* Assistant message wrapping with `›`.
* User message wrapping.
* Adjacent assistant/user messages without labels.
* Tool panel indentation.
* Composer indentation.
* Composer top-frame statusline.
* Composer context meter with exactly 10 dots.
* Composer workspace label.
* `EXPLORE` with one body line.
* `EXPLORE` with multiple body lines.
* `SHELL` panel alignment.
* `SHELL` compact duration formatting.
* `EDIT` wrapped block diff rendering.
* `EDIT` continuation row alignment.
* `EDIT` panel border integrity.
* Inline LED-chase working indicator.
* Turn divider after tool-backed turn.
* Absence of bottom status bar.
* No standalone `READ` panel for exploration.
* Equal-width panel rows.
* Narrow terminal wrapping.
* Wide terminal layout.
* Collapsed panel rendering.
* Hidden-content affordance rendering.
* Symbol vocabulary mapping for all tool states.

## Final Design Rule

The pane should feel like a precise transcript instrument.

Plain language flows lightly.

Tools become mechanical panels.

State is communicated with a small, consistent symbol vocabulary.

The current operation is a tiny LED readout.

The editor is a calm input module.

Nothing else gets chrome.
