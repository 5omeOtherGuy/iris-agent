# Iris TUI Tool Output Rendering Specs

## Scope

This document defines canonical rendering specs for Iris TUI tool outputs.

It updates the existing `SHELL` and `EDIT` specs to match the current pane design language, and defines the remaining tool-output families:

* `EXPLORE`
* `SHELL`
* `EDIT`
* `APPROVAL`
* generic/fallback `TOOL`

These specs assume the shared Iris TUI design language is authoritative.

## Shared Tool Panel Rules

All tool outputs render as bordered instrument panels.

Canonical panel anatomy:

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  TOOL  meta                                             SYMBOL STATE     ELAPSED    │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    body text                                                                          │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

Rules:

* Tool panels are indented into the transcript column.
* Tool panels align with the composer width.
* Every physical row must have the same display width.
* Never append a separator to a header row.
* Never let wrapped content escape the frame.
* Header-only panels are allowed when a tool has no meaningful body.
* Use compact elapsed durations.
* Do not use `T+`.
* Do not use fixed `HH:MM:SSs` for ordinary tool calls.

## Shared State Symbols

Use the pane-level symbol vocabulary.

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

Use the state symbol in the panel header before the state label.

Examples:

```text
│ ▾  SHELL  bash                                             ◆ DONE        45s       │
│ ▾  SHELL  bash                                             ● RUNNING     13s       │
│ ▾  SHELL  bash                                             ■ ERROR       7.1s      │
│ ▾  EDIT   path/to/file.rs                                    ◇ PREVIEW               │
│ ▾  APPROVAL apply_patch                                    ▲ REVIEW                │
```

## Shared Duration Format

Use compact elapsed duration everywhere in tool headers.

```text
< 10s       → 0.5s, 7.1s
10–59s      → 13s, 42s
1–59min     → 1:27, 12:03
>= 60min    → 1:02:14
```

Examples:

```text
◆ DONE        0.5s
◆ DONE        16s
◆ DONE        1:48
■ ERROR       7.1s
● RUNNING     13s
```

## Shared Path Rules

Prefer compact paths.

Rules:

* Replace `/home/<user>` with `~` when accurate.
* Prefer repo-relative paths when context is already clear.
* Preserve filenames when truncating.
* Use middle truncation for long paths.
* Do not let paths collide with the right-side state slot.

Good:

```text
path/to/file.rs
~/project/src/module.rs
~/project/src/…/module.rs
```

Bad:

```text
~/project/src/module_with_a_very_long_na● DONE
```

---

# EXPLORE Tool Output

## Purpose

`EXPLORE` renders all read/search/list/find-style inspection.

It is the canonical container for:

* read file
* grep/search
* list directory
* find files
* inspect definitions
* scan project structure
* locate symbols
* summarize relevant files

Do not render normal workflow `READ`, `GREP`, `LS`, or `FIND` as top-level panels.

## Header

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  EXPLORE  path/to/module                                      ◆ DONE        0.0s       │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
```

Header meta should be a compact scope, not a full raw query dump.

Good meta examples:

```text
path/to/module
docs
tmp
renderer event path
```

If no good scope exists, omit meta:

```text
│ ▾  EXPLORE                                                   ◆ DONE        0.0s       │
```

## Body

Each body row is a concise exploration action.

```text
│    Read path/to/file.rs                                                                  │
│    Search user|assistant|inline in path/to/file.rs                                       │
│    Find *test*.rs in ~/project                                                           │
```

Rules:

* Body rows start with four spaces inside the panel.
* Use human-readable verbs: `Read`, `Search`, `Find`, `List`.
* Do not prefix with bullets.
* Do not show raw tree gutters.
* Do not repeat `EXPLORE` inside the body.
* Collapse repeated identical rows.

## Repeated Rows

If the same action repeats, compact it.

Instead of:

```text
│    Read path/to/module.rs                                                           │
│    Read path/to/module.rs                                                           │
│    Read path/to/module.rs                                                           │
```

Render:

```text
│    Read path/to/module.rs ×3                                                        │
```

## Long Search Rows

Wrap search rows semantically.

Bad:

```text
│    Search inline|message|user in ~/project/src/module.rs                                 │
```

Good:

```text
│    Search inline|message|user                                                          │
│      in ~/project/src (*.rs)                                                           │
```

## Running Explore

When exploration is active:

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  EXPLORE  path/to/module                                      ● RUNNING     3.2s       │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    Search user|assistant|inline in path/to/file.rs                                      │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

If no body rows are known yet, render a header-only running panel.

## Explore Snapshot Tests

Add snapshots for:

* one read
* multiple actions
* repeated identical reads
* long search query wrapping
* full path shortening
* running explore
* header-only explore
* no top-level `READ`, `GREP`, `LS`, or `FIND` for normal workflow

---

# SHELL Tool Output

## Purpose

`SHELL` renders command execution.

It remains a top-level panel.

`SHELL` must use the same panel grammar as other tools while preserving terminal output readability.

## Header

Preferred:

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  SHELL                                                   ◆ DONE        45s         │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
```

Optional shell name may be included if useful and width allows:

```text
│ ▾  SHELL  bash                                             ◆ DONE        45s         │
```

Do not let the shell name cause truncation or state collision.

## Command Row

The first body row is the command invocation.

```text
│    $ cargo test ui::tui::tests                                      timeout 120s      │
```

Rules:

* Command row starts with four spaces and `$`.
* Timeout is metadata, not command text.
* Put timeout on the command row when available.
* Do not render `(timeout 120s)` as part of the command.
* Do not render `(no timeout)` as command text; omit timeout if none.

Bad:

```text
│ $ cargo test ui::tui::tests (timeout 120s)                                            │
```

Good:

```text
│    $ cargo test ui::tui::tests                                      timeout 120s      │
```

## Output Rows

Output rows are indented one level beneath the command.

```text
│      gate: PASS — fmt OK, clippy OK, test OK                                          │
```

Rules:

* Output rows start with six spaces inside the panel.
* Preserve meaningful blank lines.
* Do not add excessive blank rows.
* Use word-aware wrapping.
* Avoid splitting identifiers, paths, and decimals when possible.
* Keep output inside the panel.

## No Output

Render explicit no-output text.

```text
│      (no output)                                                                      │
```

## Running Shell

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  SHELL                                                   ● RUNNING     13s         │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    $ cargo test ui::tui::tests                                      timeout 120s      │
  │      Compiling project v0.1.0                                                     │
  │      █                                                                                │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

Rules:

* Use `● RUNNING`.
* Append a subtle cursor row after latest output.
* The turn-level working indicator remains separate and unframed.

## Error Shell

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  SHELL                                                   ■ ERROR       7.1s        │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    $ cargo test ui::tui::tests                                      timeout 120s      │
  │      error: test failed, to rerun pass `--bin app`                                  │
  │                                                                                      │
  │      Command exited with code 101                                                    │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

Rules:

* Use `■ ERROR`.
* Keep raw error text.
* Style obvious error lines with error color if available.
* At most one blank line before exit code.

## Hidden Output

Use the shared hidden-content affordance.

```text
│      … 316 lines hidden                                      ctrl+o to expand          │
```

Rules:

* Use `…`, not `...`.
* Use `ctrl+o to expand` / `ctrl+o to collapse`.
* Do not use `toggles panel`.
* Keep the row dim/muted.
* Keep it inside the shell panel.

## Multi-line Commands and Heredocs

Split long heredoc commands into command and payload.

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  SHELL                                                   ◆ DONE        0.5s        │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    $ cd ~/project                                              timeout 120s          │
  │    $ python3 - <<'PY'                                                                 │
  │                                                                                      │
  │      payload  python                                                                 │
  │      ─────────────────────────────────────────────────────────────────────────────    │
  │      from pathlib import Path                                                        │
  │      p = Path('path/to/file.rs')                                                     │
  │      … 5 replacements hidden                                      ctrl+o to expand    │
  │      p.write_text(s)                                                                 │
  │      PY                                                                              │
  │      cargo fmt                                                                       │
  │                                                                                      │
  │      (no output)                                                                     │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

Rules:

* Do not render huge heredoc commands as one raw `$ ...` line.
* Infer payload language when possible.
* Fold long payloads.
* Keep command structure readable.

## Shell Snapshot Tests

Add snapshots for:

* short output
* no output
* long output
* hidden output affordance
* error output
* running output
* command timeout metadata
* no timeout
* semantic command wrapping
* heredoc payload
* path shortening
* compact duration formatting

---

# EDIT Tool Output

## Purpose

`EDIT` renders file mutations and patch previews.

It must optimize for human review.

`EDIT` uses one canonical method: wrapped block diff.

Do not switch modes based on file type.

## Header

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  EDIT  path/to/file.rs                                      ◇ PREVIEW                │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
```

Applied edit:

```text
│ ▾  EDIT  path/to/file.rs                                      ◆ DONE        0.5s        │
```

Failed edit:

```text
│ ▾  EDIT  path/to/file.rs                                      ■ ERROR       0.5s        │
```

Rules:

* Tool name is `EDIT`, not `DIFF`.
* Use compact path.
* Use `◇ PREVIEW` before apply.
* Use `◆ DONE` after successful apply.
* Use `■ ERROR` on failure.
* Do not show add/remove counters in the header by default.

## Body: Wrapped Block Diff

Canonical structure:

```text
│    3  −  removed text starts here and wraps naturally at word boundaries               │
│          continuation line aligns under content                                        │
│                                                                                       │
│    3  +  added text starts here and wraps naturally at word boundaries                 │
│          continuation line aligns under content                                        │
```

Columns:

```text
line number  marker  content
```

Markers:

```text
−   removed
+   added
    unchanged
```

Rules:

* Use true minus `−`, not ASCII hyphen `-`.
* Continuation rows align under the content column.
* Continuation rows inherit parent styling.
* Wrap at word/token boundaries.
* Do not split identifiers unless unavoidable.
* Blank rows may separate removed/added blocks for readability.
* Keep all content inside the panel.

## Unchanged Rows

Unchanged rows omit the marker.

```text
│ 266     }                                                                            │
│ 267     }                                                                            │
```

## Added Rows

```text
│ 269  +  pub(super) fn push_wrapped_row_with_prefix(                                  │
│         text: &str,                                                                  │
│         style: Style,                                                                │
```

## Removed Rows

```text
│ 543  −  assert!(rendered.iter().any(|line| line == "    HI"), "{rendered:?}");        │
```

## Edit Done Summary

If an `EDIT DONE` event appears separately from the preview, compact it.

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  EDIT  path/to/file.rs                                      ◆ DONE        0.5s        │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    Applied 1 replacement                                                              │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

Prefer in-place state transition from `◇ PREVIEW` to `◆ DONE` when the UI model supports it.

## Edit Snapshot Tests

Add snapshots for:

* preview header
* done header
* error header
* compact path
* long path truncation
* added row
* removed row
* unchanged row
* continuation row alignment
* long prose wrapping
* long code wrapping
* no old/new dual table
* no `DIFF` header
* compact applied summary

---

# APPROVAL Tool Output

## Purpose

`APPROVAL` renders permission and safety/authorization review events.

It should be compact and mechanical.

It should not dominate the transcript.

## States

Use:

```text
▲ REVIEW
◆ APPROVED
■ DENIED
■ ERROR
```

## Review Pending

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  APPROVAL  apply_patch                                    ▲ REVIEW                 │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    risk             low                                                              │
  │    authorization    high                                                             │
  │    target           path/to/file.rs                                                    │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

## Approved

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  APPROVAL  apply_patch                                    ◆ APPROVED      0.0s     │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    risk             low                                                              │
  │    authorization    high                                                             │
  │    approved target  path/to/file.rs                                                    │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

## Denied

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  APPROVAL  shell command                                  ■ DENIED        0.0s     │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
  │    risk             high                                                             │
  │    reason           destructive command requires user approval                        │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

## Body Rules

* Use aligned key/value rows for structured information.
* Avoid long prose when possible.
* Preserve meaningful review message content, but summarize mechanical metadata.
* Keep target paths compact.
* Do not render raw approval paragraphs when structured fields are available.

## Approval Snapshot Tests

Add snapshots for:

* review pending
* approved
* denied
* approval with target path
* approval with long reason wrapping
* automatic approval review
* request approved
* request denied

---

# Generic / Fallback TOOL Output

## Purpose

`TOOL` is a fallback when the renderer does not recognize the tool family.

It should be rare.

The fallback must still follow the shared panel grammar.

## Header

```text
  ┌───────────────────────────────────────────────────────────────────────────────────────┐
  │ ▾  TOOL  unknown_tool                                      ◆ DONE        0.5s        │
  ├───────────────────────────────────────────────────────────────────────────────────────┤
```

## Body

Render a concise summary first.

```text
│    Ran unknown_tool with 3 arguments                                                   │
```

If structured output exists, render key/value rows.

```text
│    path      path/to/file.rs                                                             │
│    mode      preview                                                                   │
│    result    success                                                                   │
```

If only raw output exists, render it with normal panel body padding and safe wrapping.

## Fallback Rules

* Do not invent a custom layout.
* Do not leak raw JSON unless the output is explicitly JSON and useful.
* Prefer a compact summary over full raw arguments.
* If the unknown tool is clearly read/search/list/find, map it to `EXPLORE`.
* If it mutates files or produces a diff, map it to `EDIT`.
* If it executes a command, map it to `SHELL`.
* If it concerns permission/review, map it to `APPROVAL`.

## Fallback Snapshot Tests

Add snapshots for:

* unknown successful tool
* unknown failed tool
* raw output wrapping
* structured key/value output
* remapping unknown search-like tool to `EXPLORE`
* remapping unknown mutation-like tool to `EDIT`

---

# Tool Output Event Mapping

Use this mapping before rendering:

```text
read             → EXPLORE body row
grep/search      → EXPLORE body row
find             → EXPLORE body row
ls/list          → EXPLORE body row
bash/shell       → SHELL panel
write            → EDIT panel
edit/apply_patch → EDIT panel
diff preview     → EDIT panel
approval review  → APPROVAL panel
permission event → APPROVAL panel
unknown          → TOOL fallback, or remap when semantic family is clear
```

Do not render already-formatted transcript strings when structured tool data is available.

# Shared Tool Output Testing Checklist

Add or update snapshot tests for:

* state symbol mapping
* compact duration mapping
* path shortening
* header right-slot reservation
* all rows equal width
* no orphaned panel bodies
* no malformed mixed header/separator rows
* no raw standalone `READ` panels
* no `DIFF` headers
* no braille working spinner in tool output
* no `T+` durations
* no `00:00:00s` fixed durations for normal tool calls
* hidden rows use `ctrl+o to expand`
* no `ctrl+o toggles panel`
* no bottom telemetry/status row

