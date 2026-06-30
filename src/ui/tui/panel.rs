//! Bordered panel chrome, panel metadata, and edit-table rendering.

use std::time::Instant;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

use super::rows::{ChromeRow, TranscriptRow, hrule_line};
use super::text::strip_ansi_for_text;
use super::wrap::{
    display_width, line_text, pad_line_left, pad_line_right, push_wrapped_line, spans_width,
    take_spans_to_width, truncate_line,
};
use super::{
    BOX_X_PADDING, DIFF_ADD_BG, DIFF_DEL_BG, PANEL_BODY_BORDER_WIDTH, PANEL_BODY_CHROME_WIDTH,
    PANEL_BODY_SIDE_PADDING, TEXT_COLUMN_X_PADDING, border_style, dim_style, err_style, ok_style,
    panel_style, prompt_style,
};
use crate::ui::symbols;

fn panel_outer_padding(width: usize) -> usize {
    if width <= BOX_X_PADDING.saturating_mul(2).saturating_add(1) {
        0
    } else {
        BOX_X_PADDING
    }
}

fn panel_width(width: usize) -> usize {
    width
        .saturating_sub(panel_outer_padding(width).saturating_mul(2))
        .max(1)
}

pub(super) fn panel_rule_line(width: usize, left: char, right: char) -> Line<'static> {
    let outer = panel_outer_padding(width);
    let rule_width = panel_width(width);
    let rule = match rule_width {
        0 => String::new(),
        1 => left.to_string(),
        2 => format!("{left}{right}"),
        n => format!("{left}{}{right}", "─".repeat(n - 2)),
    };
    let mut line = Line::from(vec![
        Span::raw(" ".repeat(outer)),
        Span::styled(rule, border_style()),
        Span::raw(" ".repeat(outer)),
    ]);
    truncate_line(&mut line, width.max(1));
    line
}

pub(super) fn panel_header_line(
    width: usize,
    expanded: bool,
    title: &'static str,
    meta: &str,
    right: &[(String, Style)],
) -> Line<'static> {
    let panel_width = panel_width(width);
    let inner_width = panel_width.saturating_sub(2);
    let arrow = if expanded {
        symbols::EXPANDED
    } else {
        symbols::COLLAPSED
    };
    let title_width = title.len().max(7);
    let mut left = vec![
        Span::styled(format!(" {arrow}  "), dim_style()),
        // The tool family is the panel's identity. With no type-scale axis,
        // weight is the hierarchy lever (DESIGN.md: label = bold).
        Span::styled(
            format!("{title:<title_width$}"),
            panel_style().add_modifier(Modifier::BOLD),
        ),
    ];
    let meta = strip_ansi_for_text(meta);
    if !meta.is_empty() {
        left.push(Span::styled(format!("  {meta}"), dim_style()));
    }
    let right_full: Vec<Span<'static>> = right
        .iter()
        .map(|(text, style)| Span::styled(strip_ansi_for_text(text), *style))
        .collect();
    let right = take_spans_to_width(right_full, inner_width / 2);
    let right_width = spans_width(&right);
    let left = take_spans_to_width(left, inner_width.saturating_sub(right_width));
    let left_width = spans_width(&left);
    let spacer = inner_width
        .saturating_sub(left_width)
        .saturating_sub(right_width);
    let outer = panel_outer_padding(width);
    let mut spans = vec![
        Span::raw(" ".repeat(outer)),
        Span::styled("│", border_style()),
    ];
    spans.extend(left);
    spans.push(Span::styled(" ".repeat(spacer), panel_style()));
    spans.extend(right);
    spans.push(Span::styled("│", border_style()));
    spans.push(Span::raw(" ".repeat(outer)));
    let mut line = Line::from(spans);
    truncate_line(&mut line, width.max(1));
    line
}

pub(super) fn panel_body_line(
    width: usize,
    mut line: Line<'static>,
    bg: Option<Color>,
) -> Line<'static> {
    let panel_width = panel_width(width).max(PANEL_BODY_BORDER_WIDTH);
    let body_width = panel_width.saturating_sub(PANEL_BODY_CHROME_WIDTH).max(1);
    truncate_line(&mut line, body_width);
    if let Some(bg) = bg {
        apply_width_bg(&mut line, bg, body_width);
    } else {
        let used = display_width(&line_text(&line));
        if used < body_width {
            line.spans.push(Span::styled(
                " ".repeat(body_width - used),
                Style::default(),
            ));
        }
    }
    let outer = panel_outer_padding(width);
    let side_padding = " ".repeat(PANEL_BODY_SIDE_PADDING);
    let mut spans = vec![
        Span::raw(" ".repeat(outer)),
        Span::styled("│", border_style()),
        Span::styled(side_padding.clone(), panel_style()),
    ];
    spans.extend(line.spans);
    spans.push(Span::styled(side_padding, panel_style()));
    spans.push(Span::styled("│", border_style()));
    spans.push(Span::raw(" ".repeat(outer)));
    let mut line = Line::from(spans);
    truncate_line(&mut line, width.max(1));
    line
}

pub(super) fn panel_body_lines(
    width: usize,
    line: Line<'static>,
    bg: Option<Color>,
    out: &mut Vec<Line<'static>>,
) {
    let panel_width = panel_width(width).max(PANEL_BODY_BORDER_WIDTH);
    let body_width = panel_width.saturating_sub(PANEL_BODY_CHROME_WIDTH).max(1);
    let mut wrapped = Vec::new();
    push_wrapped_line(&line, body_width, None, &mut wrapped);
    for physical in wrapped {
        out.push(panel_body_line(width, physical, bg));
    }
}

/// A reasoning-rail header: `┊ ▾ THINKING` (expanded) / `┊ ▸ THINKING`
/// (collapsed), muted and indented to the transcript text column. No box —
/// reasoning is recessive; the rail is the only chrome it gets (ThinkingBlock).
pub(super) fn rail_header_line(width: usize, expanded: bool, label: &str) -> Line<'static> {
    let arrow = if expanded {
        symbols::EXPANDED
    } else {
        symbols::COLLAPSED
    };
    let mut line = Line::from(Span::styled(
        format!("{} {arrow} {label}", symbols::SEP),
        dim_style(),
    ));
    pad_line_left(&mut line, TEXT_COLUMN_X_PADDING);
    truncate_line(&mut line, width.max(1));
    line
}

pub(super) fn inset_rule_line(width: usize, label: &str) -> Line<'static> {
    let rule_width = width.saturating_sub(BOX_X_PADDING * 2).max(1);
    let mut line = hrule_line(label, rule_width);
    pad_line_left(&mut line, BOX_X_PADDING);
    pad_line_right(&mut line, BOX_X_PADDING);
    line
}

/// Apply a background fill to one already-wrapped physical line, then pad to
/// `width` with a trailing background span (ratatui only colours the cells a
/// span occupies).
pub(super) fn apply_width_bg(line: &mut Line<'static>, bg: Color, width: usize) {
    for span in &mut line.spans {
        span.style = span.style.bg(bg);
    }
    let used = display_width(&line_text(line));
    if used < width {
        line.spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PanelState {
    Running,
    Done,
    Error,
    Cancelled,
}

impl PanelState {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Running => " RUNNING",
            Self::Done => " DONE",
            Self::Error => " ERROR",
            Self::Cancelled => " CANCELLED",
        }
    }

    /// State glyph from the symbol vocabulary in docs/TUI_DESIGN_LANGUAGE.md.
    /// `●` is reserved for the live/running LED; settled states get their own
    /// glyph so the header stays legible without color.
    pub(super) fn symbol(self) -> &'static str {
        match self {
            Self::Running => symbols::RUNNING,
            Self::Done => symbols::DONE,
            Self::Error => symbols::ERROR,
            Self::Cancelled => symbols::CANCELLED,
        }
    }

    pub(super) fn dot_style(self) -> Style {
        match self {
            Self::Running => prompt_style(),
            Self::Done => ok_style(),
            Self::Error => err_style(),
            Self::Cancelled => dim_style(),
        }
    }

    pub(super) fn plain_prefix(self) -> &'static str {
        match self {
            Self::Running => "• Running",
            Self::Done => "• Ran",
            Self::Error => "✗ Ran",
            Self::Cancelled => "• Cancelled",
        }
    }
}

pub(super) fn panel_state(running: bool, failed: bool) -> PanelState {
    if running {
        PanelState::Running
    } else if failed {
        PanelState::Error
    } else {
        PanelState::Done
    }
}

pub(super) struct PanelHeaderSpec<'a> {
    pub(super) title: &'static str,
    pub(super) meta: &'a str,
    pub(super) plain_meta: &'a str,
    pub(super) state: PanelState,
    pub(super) duration: Option<std::time::Duration>,
    pub(super) started: Option<Instant>,
}

/// Render a unified diff as the edit-tool table from the visual spec:
/// old/new line columns, a marker column, then code. File headers and hunk
/// headers are structural data, not visible rows.
pub(super) fn diff_table_rows(diff: &str) -> Vec<TranscriptRow> {
    let mut out = Vec::new();
    let mut old_line = 0usize;
    let mut new_line = 0usize;
    let lines: Vec<&str> = diff.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            i += 1;
            continue;
        }
        if let Some((old_start, new_start)) = parse_hunk_header(line) {
            old_line = old_start;
            new_line = new_start;
            i += 1;
            continue;
        }
        let Some(marker) = line.chars().next() else {
            i += 1;
            continue;
        };
        let code = line.get(marker.len_utf8()..).unwrap_or_default();
        match marker {
            '-' => {
                // Gather the consecutive removed/added run so a clean 1-for-1
                // modification can be highlighted at token granularity, matching
                // pi-mono's diff renderer.
                let mut removed: Vec<&str> = vec![code];
                i += 1;
                while let Some(next) = lines.get(i) {
                    if next.starts_with("--- ") || !next.starts_with('-') {
                        break;
                    }
                    removed.push(next.get('-'.len_utf8()..).unwrap_or_default());
                    i += 1;
                }
                let mut added: Vec<&str> = Vec::new();
                while let Some(next) = lines.get(i) {
                    if next.starts_with("+++ ") || !next.starts_with('+') {
                        break;
                    }
                    added.push(next.get('+'.len_utf8()..).unwrap_or_default());
                    i += 1;
                }
                if removed.len() == 1 && added.len() == 1 {
                    let (old_spans, new_spans) = intra_line_diff(removed[0], added[0]);
                    out.push(diff_span_row(
                        Some(old_line),
                        None,
                        symbols::REMOVED,
                        removed[0],
                        old_spans,
                        err_style(),
                        Some(DIFF_DEL_BG),
                    ));
                    old_line += 1;
                    out.push(diff_span_row(
                        None,
                        Some(new_line),
                        symbols::ADDED,
                        added[0],
                        new_spans,
                        ok_style(),
                        Some(DIFF_ADD_BG),
                    ));
                    new_line += 1;
                } else {
                    for code in removed {
                        out.push(diff_plain_row(
                            Some(old_line),
                            None,
                            symbols::REMOVED,
                            code,
                            err_style(),
                            Some(DIFF_DEL_BG),
                        ));
                        old_line += 1;
                    }
                    for code in added {
                        out.push(diff_plain_row(
                            None,
                            Some(new_line),
                            symbols::ADDED,
                            code,
                            ok_style(),
                            Some(DIFF_ADD_BG),
                        ));
                        new_line += 1;
                    }
                }
            }
            '+' => {
                out.push(diff_plain_row(
                    None,
                    Some(new_line),
                    symbols::ADDED,
                    code,
                    ok_style(),
                    Some(DIFF_ADD_BG),
                ));
                new_line += 1;
                i += 1;
            }
            ' ' => {
                out.push(diff_plain_row(
                    Some(old_line),
                    Some(new_line),
                    " ",
                    code,
                    panel_style(),
                    None,
                ));
                old_line += 1;
                new_line += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// Count content additions/removals in a unified diff, ignoring the `+++ `/`--- `
/// file headers and `@@` hunk headers. Used for the quiet EDIT footer.
pub(super) fn diff_counts(diff: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with("+++ ") || line.starts_with("--- ") {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => added += 1,
            Some(b'-') => removed += 1,
            _ => {}
        }
    }
    (added, removed)
}

/// The quiet `+added  −removed` footer that closes an EDIT panel body, tinted to
/// the diff inks (additions green, removals red) per the `EditOutput`
/// design-system component. `note` adds a `┊ <note>` aside (e.g. `new file`).
/// Symbol + color together carry the meaning; the counts read in monochrome.
pub(super) fn diff_footer_row(added: usize, removed: usize, note: Option<&str>) -> TranscriptRow {
    let mut plain = format!("{}{added}  {}{removed}", symbols::ADDED, symbols::REMOVED);
    let mut spans = vec![
        Span::styled(format!("{}{added}", symbols::ADDED), ok_style()),
        Span::styled("  ", panel_style()),
        Span::styled(format!("{}{removed}", symbols::REMOVED), err_style()),
    ];
    if let Some(note) = note {
        plain.push_str(&format!("  {} {note}", symbols::SEP));
        spans.push(Span::styled(
            format!("  {} {note}", symbols::SEP),
            dim_style(),
        ));
    }
    TranscriptRow::chrome_with_text(
        ChromeRow::Body {
            line: Line::from(spans),
            bg: None,
        },
        plain,
        panel_style(),
    )
}

/// Build a diff table row whose code column is a single styled span.
fn diff_plain_row(
    old: Option<usize>,
    new: Option<usize>,
    marker: &str,
    code: &str,
    style: Style,
    bg: Option<Color>,
) -> TranscriptRow {
    let row = format_diff_table_row(old, new, marker, code);
    TranscriptRow::chrome_with_text(
        ChromeRow::Body {
            line: Line::from(Span::styled(row.clone(), style)),
            bg,
        },
        row,
        style,
    )
}

/// Build a diff table row whose code column carries per-token spans so changed
/// words can be emphasised within an otherwise unchanged line.
fn diff_span_row(
    old: Option<usize>,
    new: Option<usize>,
    marker: &str,
    code: &str,
    code_spans: Vec<Span<'static>>,
    style: Style,
    bg: Option<Color>,
) -> TranscriptRow {
    let gutter = diff_table_gutter(old, new, marker);
    let plain = format!("{gutter}{code}");
    let mut spans = vec![Span::styled(gutter, style)];
    spans.extend(code_spans);
    TranscriptRow::chrome_with_text(
        ChromeRow::Body {
            line: Line::from(spans),
            bg,
        },
        plain,
        style,
    )
}

/// Word-level diff of a single modified line. Equal tokens keep the line's base
/// colour; changed tokens are emphasised with a reversed modifier. Whitespace-
/// only tokens are never emphasised so indentation changes stay quiet.
fn intra_line_diff(old: &str, new: &str) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let diff = TextDiff::from_words(old, new);
    let mut old_spans = Vec::new();
    let mut new_spans = Vec::new();
    for change in diff.iter_all_changes() {
        let value = change.value();
        match change.tag() {
            ChangeTag::Delete => push_token(&mut old_spans, value.to_string(), err_style()),
            ChangeTag::Insert => push_token(&mut new_spans, value.to_string(), ok_style()),
            ChangeTag::Equal => {
                old_spans.push(Span::styled(value.to_string(), err_style()));
                new_spans.push(Span::styled(value.to_string(), ok_style()));
            }
        }
    }
    (old_spans, new_spans)
}

fn push_token(spans: &mut Vec<Span<'static>>, value: String, base: Style) {
    let style = if value.trim().is_empty() {
        base
    } else {
        base.add_modifier(Modifier::REVERSED)
    };
    spans.push(Span::styled(value, style));
}

fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_part, rest) = rest.split_once(" +")?;
    let (new_part, _) = rest.split_once(" @@")?;
    Some((parse_hunk_start(old_part), parse_hunk_start(new_part)))
}

fn parse_hunk_start(part: &str) -> usize {
    part.split(',')
        .next()
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(0)
}

/// The edit-table gutter from the visual spec (docs/TUI_DESIGN_LANGUAGE.md
/// §EDIT): a single line-number column, the marker, then the content column.
/// Removal rows carry the old line number, additions/context the new one — so a
/// 1-for-1 modification shows the same number on both sides, as the spec mock
/// does. No second number column and no `|` separator.
fn diff_table_gutter(old: Option<usize>, new: Option<usize>, marker: &str) -> String {
    let num = new
        .or(old)
        .map_or_else(String::new, |line| line.to_string());
    format!("{num:>4}  {marker}  ")
}

fn format_diff_table_row(
    old: Option<usize>,
    new: Option<usize>,
    marker: &str,
    code: &str,
) -> String {
    format!("{}{code}", diff_table_gutter(old, new, marker))
}
