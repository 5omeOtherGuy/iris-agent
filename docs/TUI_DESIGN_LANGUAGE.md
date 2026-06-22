# TUI Design Language

This document is the concrete layout contract for the Iris terminal UI. Keep new
TUI surfaces aligned to these constants instead of inventing local padding.

## Core Geometry

```rust
const USER_BG: Color = Color::Rgb(50, 50, 56);

const X_PADDING: usize = 2;
const BOX_X_PADDING: usize = X_PADDING;  // terminal edge -> grey box edge
const TEXT_X_PADDING: usize = X_PADDING; // grey box edge -> text

// Terminal edge -> all readable text.
const TEXT_COLUMN_X_PADDING: usize = BOX_X_PADDING + TEXT_X_PADDING; // 4

const BOX_X_PADDING_U16: u16 = X_PADDING as u16;
const TEXT_X_PADDING_U16: u16 = X_PADDING as u16;
```

All readable transcript/chrome text starts in the same column:

```text
terminal x=0
|-- 2 cols --| grey box edge
|-- 2 cols --| readable text column (x=4)
```

Rules:

```rust
fn x_for_box() -> u16 { BOX_X_PADDING_U16 }          // 2
fn x_for_text() -> u16 { TEXT_COLUMN_X_PADDING as u16 } // 4

fn width_for_box(term_width: u16) -> u16 {
    term_width.saturating_sub(BOX_X_PADDING_U16 * 2).max(1)
}

fn width_for_text(term_width: u16) -> u16 {
    term_width
        .saturating_sub(TEXT_COLUMN_X_PADDING as u16)
        .max(1)
}
```

## Transcript Row Padding

Every transcript row is either boxed, unboxed content, or a separator.

```rust
fn row_text_padding(row: &TranscriptRow) -> usize {
    if row.background.is_some() {
        usize::from(!row.text.is_empty()) * TEXT_X_PADDING
    } else if is_separator_row(row) {
        0
    } else {
        TEXT_COLUMN_X_PADDING
    }
}
```

Meaning:

- Boxed rows paint from `x=2` to `term_width - 2`; their non-empty text starts at
  `x=4`.
- Unboxed assistant text, tool output, tool summaries, slash menu rows, working
  indicator text, and footer text start at `x=4`.
- Separator rows are truly empty and unpadded.

Do not add per-message padding. Route new transcript output through the row
model and let `row_text_padding` apply the column.

## Vertical Rhythm

Top-level blocks get one blank separator row before the next block.

```rust
fn push_blank(&mut self) {
    self.exploring_open = false;
    match self.rows.last() {
        None => {}
        Some(last) if is_separator_row(last) => {}
        _ => self
            .rows
            .push(TranscriptRow::new(String::new(), Style::default())),
    }
}
```

Boxed blocks own their internal vertical padding. Unboxed blocks do not paint
their blank rows.

```text
[previous block]

  boxed user/editor top pad row
    readable text
  boxed user/editor bottom pad row

    unboxed assistant/tool/slash/working/footer text
```

## User Message Blocks

Submitted user messages render as shaded blocks with no prompt glyph.

```rust
fn commit_user(&mut self, text: &str) {
    self.push_blank();
    self.push_user_pad();
    for line in text.split('\n') {
        let spans = vec![Span::raw(line.to_string())];
        self.rows
            .push(TranscriptRow::with_line(Line::from(spans), None).with_bg(USER_BG));
    }
    self.push_user_pad();
}

fn push_user_pad(&mut self) {
    self.rows
        .push(TranscriptRow::with_line(Line::default(), None).with_bg(USER_BG));
}
```

Shape:

```text
  <USER_BG full box width, empty>
    user text
  <USER_BG full box width, empty>
```

The box begins at `x=2`; the user text begins at `x=4`.

## Assistant And Tool Output

Assistant replies and rendered tool output are unboxed transcript content. They
start at `x=4` and rely on block separators for vertical spacing.

```rust
fn push_unboxed_content(row: TranscriptRow) {
    // row.background == None
    // row_text_padding(row) == TEXT_COLUMN_X_PADDING
}
```

No colored container. No local `Span::raw("    ")` padding. No prompt glyph.

## Working Indicator

The live working indicator is fixed chrome above the editor. It is unboxed and
has one blank row above and one blank row below.

```rust
fn working_lines(
    glyph: &str,
    elapsed: Option<Duration>,
    footer: Option<&Footer>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut line = Line::from(working_spans(glyph, elapsed, footer));
    truncate_line(&mut line, content_width(width));
    pad_line_left(&mut line, TEXT_COLUMN_X_PADDING);
    vec![Line::default(), line, Line::default()]
}
```

Text format:

```rust
fn working_spans(
    glyph: &str,
    elapsed: Option<Duration>,
    footer: Option<&Footer>,
) -> Vec<Span<'static>> {
    let secs = elapsed.map_or(0, |d| d.as_secs());
    let mut details = vec![format_elapsed_compact(secs)];
    if let Some(usage) = footer.and_then(|footer| footer.usage.as_ref()) {
        details.push(format!(
            "\u{2193} {} tokens",
            compact_count(usage.total_tokens)
        ));
    }
    if let Some(effort) = footer.and_then(|footer| footer.effort.as_ref()) {
        details.push(format!("thinking with {effort} effort"));
    }
    let suffix = format!(" ({})", details.join(" \u{b7} "));
    vec![
        Span::styled(format!("{glyph} "), prompt_style()),
        Span::styled("Working\u{2026}", dim_style()),
        Span::styled(suffix, dim_style()),
    ]
}
```

## Editor Box

The editor is a shaded box matching committed user messages.

```rust
const MIN_EDITOR_H: u16 = 3; // top pad + one text row + bottom pad

let box_area = Rect {
    x: editor_area.x + BOX_X_PADDING_U16.min(editor_area.width.saturating_sub(1)),
    y: editor_area.y,
    width: editor_area
        .width
        .saturating_sub(BOX_X_PADDING_U16 * 2)
        .max(1),
    height: editor_area.height,
};

Block::default()
    .style(Style::default().bg(USER_BG))
    .render(box_area, &mut buf);

let pad = u16::from(editor_area.height >= 3);
let text_area = Rect {
    x: box_area.x + TEXT_X_PADDING_U16.min(box_area.width.saturating_sub(1)),
    y: editor_area.y + pad,
    width: box_area.width.saturating_sub(TEXT_X_PADDING_U16 * 2).max(1),
    height: editor_area.height.saturating_sub(pad * 2).max(1),
};
```

Shape:

```text
  <USER_BG full box width, empty>
    Type a message, / for commands, Enter to send
  <USER_BG full box width, empty>
```

There is no permanent blank row above the editor. During an active turn, the
working indicator supplies the visual separation between transcript and editor.

## Above-Editor Menus

Slash commands and command-owned menus are plain unboxed chrome above the
working/editor area. They have no border, no title frame, and no grey
background.

```rust
const MAX_MENU_ROWS: u16 = 16; // includes the blank row above and below

let inner = Rect {
    x: area.x + TEXT_COLUMN_X_PADDING as u16,
    y: area.y + u16::from(area.height > 1),
    width: area.width.saturating_sub(TEXT_COLUMN_X_PADDING as u16).max(1),
    height: area.height.saturating_sub(2).max(1),
};
```

Generic modal/menu rows use the same geometry:

```rust
fn render_plain_menu_lines(buf: &mut Buffer, area: Rect, lines: Vec<Line<'static>>) {
    let inner = Rect {
        x: area.x + TEXT_COLUMN_X_PADDING as u16,
        y: area.y + u16::from(area.height > 1),
        width: area.width.saturating_sub(TEXT_COLUMN_X_PADDING as u16).max(1),
        height: area.height.saturating_sub(2).max(1),
    };
    Paragraph::new(Text::from(lines)).render(inner, buf);
}
```

Slash palette rows align command descriptions to one column:

```rust
let command_width = matches
    .iter()
    .map(|cmd| display_width(cmd.name))
    .max()
    .unwrap_or(0);

for (i, cmd) in matches.iter().enumerate() {
    let selected = i == selected_index;
    let name_style = if selected {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let description_style = if selected {
        Style::default().fg(Color::Cyan)
    } else {
        dim_style()
    };
    let gap = command_width
        .saturating_sub(display_width(cmd.name))
        .saturating_add(2);

    Line::from(vec![
        Span::styled(cmd.name.to_string(), name_style),
        Span::raw(" ".repeat(gap)),
        Span::styled(cmd.description, description_style),
    ]);
}
```

Expected shape:

```text
    /exit           End the session
    /model          Show or switch provider/model
    /reasoning      Set reasoning effort [off|minimal|low|medium|high|xhigh]
    /scoped-models  Enable/disable models for Ctrl+P cycling
    /settings       Open settings menu
    /login          Configure provider authentication
    /logout         Remove provider authentication
```

Selection is color only:

- selected command: cyan + bold
- selected description: cyan
- unselected command: default foreground
- unselected description: dim
- no selected-row background
- no leading space before `/`

Model/settings/login/logout menus follow the same rule: selected rows are
foreground color only, unselected primary labels use the default foreground, and
secondary details are dim. The editor remains visible below every menu.

## Footer Statusline

The footer is unboxed and text-column aligned below the editor.

```rust
fn footer_lines(footer: &Footer, width: usize) -> Vec<Line<'static>> {
    let width = content_width(width);
    let model = truncate_to_width(&footer.model, width);
    let model_width = display_width(&model);
    let usage = footer.usage.as_ref().map(footer_usage_text);
    let usage = usage.as_deref().unwrap_or_default();
    let usage_max = width.saturating_sub(model_width).saturating_sub(1);
    let usage = if usage_max > 0 {
        truncate_to_width(usage, usage_max)
    } else {
        String::new()
    };
    let usage_width = display_width(&usage);
    let pad = width
        .saturating_sub(usage_width)
        .saturating_sub(model_width);

    let mut second = Vec::new();
    if !usage.is_empty() {
        second.push(Span::styled(usage, dim_style()));
    }
    second.push(Span::raw(" ".repeat(pad)));
    second.push(Span::styled(model, Style::default().fg(Color::Cyan)));

    vec![
        Line::from(Span::styled(
            truncate_to_width(&footer.cwd, width),
            Style::default().fg(Color::Green),
        )),
        Line::from(second),
    ]
}

fn pad_content_lines(lines: &mut [Line<'static>]) {
    for line in lines {
        if !line_text(line).is_empty() {
            pad_line_left(line, TEXT_COLUMN_X_PADDING);
        }
    }
}
```

Shape:

```text
    ~/projects/iris-agent (branch)
    12.4k tokens                                      opus-4.5 high
```

## Bottom Chrome Order

Bottom chrome is always reserved in this order:

```rust
Layout::vertical([
    Constraint::Length(menu_h),
    Constraint::Length(working_h),
    Constraint::Length(editor_h),
    Constraint::Length(status_h),
])
```

Invariants:

- The editor is never starved by a tall menu.
- Slash and command menus appear above the working indicator/editor.
- The working indicator appears immediately above the editor during active work.
- The statusline appears below the editor.

## Implementation Checklist

For any new TUI surface:

```rust
assert_eq!(box_left, 2);
assert_eq!(readable_text_left, 4);
assert!(unboxed_rows_have_no_background);
assert!(selected_rows_use_foreground_color_only);
assert!(separator_rows_are_empty_and_unpadded);
assert!(boxed_rows_paint_from_x_2_and_text_starts_at_x_4);
```

Do not add a new padding constant unless the visual language itself changes.
