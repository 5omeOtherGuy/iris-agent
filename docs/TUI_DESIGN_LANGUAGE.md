# Iris TUI Pane Design Language & Rendering Spec

## Purpose

This document defines the ground-truth visual and structural design language for the Iris TUI main pane.

It is intended as a guide for coding agents and future design sessions. It does not attempt to fully specify every individual tool output format. Instead, it defines the shared pane grammar that all future tool renderers, transcript messages, working indicators, and input editors must follow.

The goal is a terminal-native coding-agent interface that feels calm, precise, minimal, mechanical, and readable. The visual direction is inspired by Teenage Engineering-style industrial design: restrained grey palette, functional typography, sparse accent color, clear instrument-like panels, and no unnecessary chrome.

## Design Summary

The pane is a single vertically scrolling transcript column with a fixed multiline composer at the bottom.

The transcript contains:

* Plain assistant/agent messages.
* Plain user messages.
* Bordered tool output panels.
* Minimal inline working indicators.
* Optional future structured sections.

The pane does **not** use chat-style role cards. It does **not** label every message with `USER` or `AGENT`. It does **not** use a bottom telemetry/status bar. It does **not** create framed panels for transient working states.

The visual hierarchy is:

```text
tool panel
tool panel

  ●   assistant message
      wrapped assistant line

      user message
      wrapped user line

  ●   assistant message

tool panel

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
  ●   I listed the available tools and tested bash, read, write, edit, grep,
      find, and ls in the temporary directory.

┌───────────────────────────────────────────────────────────────────────────────────────┐
│ ▾  EXPLORE  tmp                                              ● DONE        00:00:00s  │
├───────────────────────────────────────────────────────────────────────────────────────┤
│    List /home/someotherguy/tmp                                                        │
└───────────────────────────────────────────────────────────────────────────────────────┘
```

Bad:

```text
┌───────────────────────────────────────────────────────────────────────────────────────┐
│  AGENT  │ I listed the available tools...                                             │
└───────────────────────────────────────────────────────────────────────────────────────┘
```

### 3. No explicit `AGENT` / `USER` role labels

The current pane direction removes visible role labels.

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

* Assistant/agent output uses a small `●` marker.
* User text appears as plain transcript text aligned to the transcript text column.
* Tool panels provide their own identity via headers such as `SHELL`, `EXPLORE`, `EDIT`, `APPROVAL`.

This makes the pane feel like a CLI transcript rather than a chat application.

### 4. The `●` marker defines the transcript rhythm

Assistant messages use a small dot marker:

```text
  ●   Done; I listed the available tools and tested read, write, edit, grep,
      find, ls, and bash in `/home/someotherguy/tmp`.
```

Rules:

* The dot sits in the transcript marker column.
* Assistant text starts after the dot and spacing.
* Wrapped assistant lines align with the first text column, not with the dot.
* The dot should be visually subtle but recognizable.
* In color-capable themes, the active/current assistant dot may use the accent color; completed/plain assistant dots may be neutral.

Recommended shape:

```text
  ●   first line
      wrapped line
      wrapped line
```

Do not use a framed assistant message.

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

If future ambiguity becomes a real problem, a subtle user marker may be introduced later, but do not add one by default in this spec.

### 6. Tool panels are indented into the transcript column

Tool panels do not start at absolute column zero.

They are indented so they belong to the same visual transcript system as the `●` marker and text.

Recommended pattern:

```text
    ┌───────────────────────────────────────────────────────────────────────────────────┐
    │ ▾  EXPLORE  tmp                                          ● DONE        00:00:00s  │
    ├───────────────────────────────────────────────────────────────────────────────────┤
    │    Find *.txt in /home/someotherguy/tmp                                           │
    │    List /home/someotherguy/tmp                                                    │
    └───────────────────────────────────────────────────────────────────────────────────┘
```

This makes tool panels feel like transcript events, not full-screen cards.

### 7. Composer aligns with tool panels

The editor/composer is also indented into the same content column.

It should align with the left edge and width of tool panels.

Recommended:

```text
    ┌──────────────────────────────────────────────────────────────────────────────────┐
    │                                                                                  │
    │  Give iris a task...                                                             │
    │                                                                                  │
    │ ↵ to send  •  shift+↵ for new line  •  / for commands                            │
    └──────────────────────────────────────────────────────────────────────────────────┘
```

The composer is not a one-line prompt. It is a multiline editor.

## Pane Anatomy

The main pane has three conceptual regions:

```text
scrolling transcript
minimal inline working indicator, when active
fixed multiline composer
```

There is no bottom telemetry/status bar.

A compact top status line may exist elsewhere in the product, but it is outside the scope of this pane spec. The pane itself should not duplicate global state.

## Transcript Layout Grid

Use a small fixed left gutter and a content column.

Recommended conceptual columns:

```text
columns 0..1    outer padding / terminal margin
column 2        assistant marker column
columns 3..5    marker-to-text gap
column 6        transcript text column
column 4        tool/composer left edge, depending on renderer constraints
```

The exact numeric columns may vary by terminal width, but the visual relationship must hold:

* Assistant marker appears slightly left of assistant text.
* Assistant wrapped lines align under assistant text.
* User text aligns with assistant text.
* Tool panels and composer are indented so they feel attached to the same transcript column.
* Tool panel interior content has its own padding.

## Natural-Language Message Rendering

### Assistant message

Assistant messages render as plain text with a dot marker.

```text
  ●   You’re right — I should have followed that instruction exactly.
      I can continue and do it your way; send the next step.
```

Rules:

* Start with `●`.
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
  ●   First assistant paragraph.
      Wrapped line.

      User reply.

  ●   Second assistant paragraph.
```

Avoid cramped transcript output where messages and panels run together without breathing room.

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
│ ▾  EDIT ...                                                ● RUNNING 00:00:13s │
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
│ ▾  TOOL  meta                                             ● STATE       00:00:00s  │
```

Fields:

```text
▾          expanded disclosure marker
TOOL       uppercase tool family
meta       target, scope, path, or short summary
●          state dot
STATE      state label
00:00:00s  fixed-width elapsed duration
```

Do not use `T+` in pane-level framed tool headers unless explicitly reintroduced later. Current pane-level duration format is:

```text
00:00:00s
00:00:13s
00:01:48s
01:12:09s
```

Use one duration format consistently across all framed tool panels.

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
```

Optional future states:

```text
QUEUED
TIMEOUT
SKIPPED
```

Use only states that correspond to real execution state.

### State dot

The state dot is part of the mechanical readout.

Use the same glyph:

```text
●
```

Color by state where color is available:

* `RUNNING`: orange accent
* `DONE`: muted success / neutral
* `ERROR`: muted red
* `CANCELLED`: muted gray
* `APPROVED`: success or neutral
* `DENIED`: red or amber

The state label must remain visible so the interface works without color.

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
    ┌───────────────────────────────────────────────────────────────────────────────────┐
    │ ▾  EXPLORE  tmp                                          ● DONE        00:00:00s  │
    ├───────────────────────────────────────────────────────────────────────────────────┤
    │    Find *.txt in /home/someotherguy/tmp                                           │
    │    List /home/someotherguy/tmp                                                    │
    └───────────────────────────────────────────────────────────────────────────────────┘
```

Bad:

```text
    ┌───────────────────────────────────────────────────────────────────────────────────┐
    │ ▾  READ  /home/someotherguy/tmp/file.txt                 ● DONE        00:00:00s  │
    └───────────────────────────────────────────────────────────────────────────────────┘
```

Top-level `READ` may only be considered if the product later introduces a user-facing primary read action. For current agent workflow, `READ` belongs to `EXPLORE`.

### SHELL

`SHELL` is for command execution.

It remains a top-level panel.

The shell panel uses the same framed panel grammar as other tools:

```text
    ┌───────────────────────────────────────────────────────────────────────────────────┐
    │ ▾  SHELL                                                ● DONE        00:00:00s  │
    ├───────────────────────────────────────────────────────────────────────────────────┤
    │    $ command                                                         timeout 120s │
    │      output                                                                        │
    └───────────────────────────────────────────────────────────────────────────────────┘
```

Shell-specific command/output rules should follow the shell output rendering spec, but the shell panel must still obey this pane-level spec:

* indented panel
* no bottom status bar
* consistent header duration
* no framed working panel
* body content aligned with tool body padding

### EDIT

`EDIT` is for file mutations and patch previews.

It remains a top-level panel.

`EDIT` panels should visually focus on the changed lines, not on verbose metadata.

Expected body style:

* old line column
* new line column
* change marker column
* code column
* muted unchanged rows
* distinct added/removed styling in color-capable themes

The `EDIT` header should identify the target file.

Do not use `DIFF` as the top-level tool family when the event semantically represents an edit. Diff rendering is the body presentation of an `EDIT`.

Good:

```text
│ ▾  EDIT  ~/project/src/ui/tui.rs                         ● DONE        00:00:00s  │
```

Bad:

```text
│ ▾  DIFF  ~/project/src/ui/tui.rs                         ● DONE        00:00:00s  │
```

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

### WORKING

Working state is **not** a framed panel.

Do not render:

```text
    ┌───────────────────────────────────────────────────────────────────────────────────┐
    │ ▾  WORKING                                             ● RUNNING     00:06:00s  │
    ├───────────────────────────────────────────────────────────────────────────────────┤
    │    esc to interrupt                                                               │
    └───────────────────────────────────────────────────────────────────────────────────┘
```

Working state is an inline status readout.

Recommended shape:

```text
  ⠋  1m 27s  •  esc to interrupt  •  ↑177k  ↓5.7k
```

The word `Working` is optional and usually redundant. The spinner itself communicates activity.

Spinner frames:

```text
⠋
⠙
⠹
⠸
⠼
⠴
⠦
⠧
⠇
⠏
```

Rules:

* Render the working indicator inline, not boxed.
* Align it with transcript flow.
* Use the spinner as the activity signal.
* Include elapsed time.
* Include interrupt hint.
* Include token/IO telemetry if available.
* Keep it to one line.
* Do not duplicate this information in a bottom status bar.

## Composer / Editor

The composer is a large bordered multiline editor at the bottom of the pane.

It aligns with tool panels.

Canonical structure:

```text
    ┌──────────────────────────────────────────────────────────────────────────────────┐
    │                                                                                  │
    │  Give iris a task...                                                             │
    │                                                                                  │
    │ ↵ to send  •  shift+↵ for new line  •  / for commands                            │
    └──────────────────────────────────────────────────────────────────────────────────┘
```

### Composer rules

* Always bordered.
* Always multiline.
* Taller than one row.
* Indented to align with tool panels.
* No separate bottom status bar.
* Placeholder text inside editor.
* Hints inside editor.
* Hints are subtle and dim.
* Editor should not look like a chat bubble.
* Editor should feel like an input instrument.

### Placeholder text

Use product-specific language:

```text
Give iris a task...
```

Avoid generic assistant language:

```text
Ask the agent anything...
```

### Hint row

Use concise inline hints:

```text
↵ to send  •  shift+↵ for new line  •  / for commands
```

Do not move these hints into a separate bottom bar.

### Composer height

Default composer height should be at least 4 rows including borders.

Suggested minimum:

```text
top border
blank/input row
blank row
hint row
bottom border
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

This information was visually noisy and not relevant to the current pane direction.

If global runtime metadata is needed, place it in a compact top status line outside the transcript pane or expose it through a command/help overlay.

### Top status line

A compact top status line may exist elsewhere in the product. It should not be duplicated inside the pane.

If present, it should be minimal and global:

```text
● active  ┊  mode code  ┊  approval auto  ┊  branch main
```

But the pane design must not depend on it.

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
    └───────────────────────────────────────────────────────────────────────────────────┘

  ●   Done; I listed the available tools...
```

### Between message turns

Use one blank line between distinct turns or paragraphs.

```text
  ●   Assistant message.

      User response.

  ●   Assistant response.
```

### Inside panels

Use compact padding.

Body lines start with four spaces inside the panel by default:

```text
│    body text
```

For shell command output, command/output indentation may use:

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
* Long paths may be shortened only when safe and unambiguous.

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
* subtle orange accent for active/running
* muted green for success
* muted red/dusty rose for errors
* pale green/red backgrounds for edit diff rows when available

### Dark mode direction

Use:

* deep graphite / dark grey backgrounds
* warm grey panels
* soft grey borders
* off-white text
* vivid orange accent for active/running
* muted sage green for success/additions
* dusty red/rose for errors/removals

### Color restraint

Color should be sparse.

Use color for:

* active dot
* running spinner
* state labels/dots
* diff additions/removals
* warnings/errors

Do not color entire panels aggressively.

The UI must remain understandable without color.

## Interaction Model

### Expand/collapse panels

Use the disclosure marker in panel headers.

```text
▾ expanded
▸ collapsed
```

Collapsed panel:

```text
    ┌───────────────────────────────────────────────────────────────────────────────────┐
    │ ▸  EXPLORE  tmp                                          ● DONE        00:00:00s  │
    └───────────────────────────────────────────────────────────────────────────────────┘
```

Expanded panel:

```text
    ┌───────────────────────────────────────────────────────────────────────────────────┐
    │ ▾  EXPLORE  tmp                                          ● DONE        00:00:00s  │
    ├───────────────────────────────────────────────────────────────────────────────────┤
    │    List /home/someotherguy/tmp                                                    │
    └───────────────────────────────────────────────────────────────────────────────────┘
```

### Hidden long content

When content is hidden or folded, use a single subtle affordance row.

```text
│      … 11 earlier lines hidden                                      ctrl+o to expand  │
```

Rules:

* Use `…`, not `...`.
* Show hidden count.
* Right-align the expansion hint if possible.
* Keep it inside the relevant panel.
* Do not leak raw hidden content after the panel.

### Working indicator

The working indicator is not expandable and not framed.

It is a live line in the transcript flow.

```text
  ⠋  1m 27s  •  esc to interrupt  •  ↑177k  ↓5.7k
```

## Event-to-Render Mapping

### Assistant text event

Render as:

```text
  ●   assistant text
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
    │ ▾  EXPLORE  scope                                     ● DONE        00:00:00s
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

Use a diff table inside the body.

### Approval event

Render as a bordered `APPROVAL` panel.

Keep content concise.

### Working event

Render as an inline spinner readout.

Do not use a panel.

### Composer

Render as fixed bottom multiline editor.

## Do / Don’t

### Do

* Use a single transcript column.
* Use assistant `●` marker.
* Keep natural-language messages unboxed.
* Indent tool panels.
* Align composer with tool panels.
* Use hard borders only for tool panels and composer.
* Use `EXPLORE` as the container for read/search/list/find.
* Use `SHELL` for command execution.
* Use `EDIT` for mutation/diff previews.
* Use inline spinner for working state.
* Keep all panel rows width-safe.
* Keep the bottom of the pane clean.

### Don’t

* Do not render `USER` / `AGENT` labels.
* Do not box user or assistant messages.
* Do not create standalone `READ` panels for normal exploration.
* Do not render a framed `WORKING` panel.
* Do not include a bottom telemetry/status row.
* Do not make short outputs use a totally different visual system.
* Do not leak raw bullet output outside panels.
* Do not append separators to header rows.
* Do not rely on color alone.
* Do not over-decorate with icons.

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

Implement this spec incrementally. Prefer the smallest coherent slice that can be tested, such as assistant/user message shape, one panel family, working indicator, or composer geometry. Avoid broad rewrites that combine visual changes with runtime, tool execution, session storage, or provider-contract changes.

## Testing Requirements

Add golden/snapshot tests for:

* Assistant message wrapping.
* User message wrapping.
* Adjacent assistant/user messages without labels.
* Tool panel indentation.
* Composer indentation.
* `EXPLORE` with one body line.
* `EXPLORE` with multiple body lines.
* `SHELL` panel alignment.
* `EDIT` panel border integrity.
* Inline working indicator.
* Absence of bottom status bar.
* No standalone `READ` panel for exploration.
* Equal-width panel rows.
* Narrow terminal wrapping.
* Wide terminal layout.
* Collapsed panel rendering.
* Hidden-content affordance rendering.

## Final Design Rule

The pane should feel like a precise transcript instrument.

Plain language flows lightly.

Tools become mechanical panels.

The current operation is a tiny live readout.

The editor is a calm input module.

Nothing else gets chrome.
