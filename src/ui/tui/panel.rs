//! Bordered panel chrome, panel metadata, and edit-table rendering.

use std::time::Instant;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::nexus::ToolCall;
use crate::tool_display::{display_path, exploration_summary, summarize};

use super::rows::{ChromeRow, TranscriptRow, hrule_line};
use super::text::strip_ansi_for_text;
use super::wrap::{
    display_width, line_text, pad_line_left, pad_line_right, push_wrapped_line, spans_width,
    take_spans_to_width, truncate_line,
};
use super::{
    BOX_X_PADDING, DIFF_ADD_BG, DIFF_DEL_BG, PANEL_BODY_BORDER_WIDTH, PANEL_BODY_CHROME_WIDTH,
    PANEL_BODY_SIDE_PADDING, border_style, dim_style, err_style, ok_style, panel_style,
    prompt_style,
};

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
    let arrow = if expanded { "▾" } else { "▸" };
    let title_width = title.len().max(7);
    let mut left = vec![
        Span::styled(format!(" {arrow}  "), dim_style()),
        Span::styled(format!("{title:<title_width$}"), panel_style()),
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

pub(super) fn tool_panel_title(call: &ToolCall) -> &'static str {
    match call.name.as_str() {
        "read" | "grep" | "find" | "ls" => "EXPLORE",
        "write" | "edit" => "EDIT",
        _ => "TOOL",
    }
}

fn tool_path_arg(call: &ToolCall) -> Option<&str> {
    call.arguments
        .get("file_path")
        .or_else(|| call.arguments.get("path"))
        .and_then(|value| value.as_str())
}

pub(super) fn tool_panel_meta(call: &ToolCall) -> String {
    tool_path_arg(call)
        .map(display_path)
        .unwrap_or_else(|| summarize(call))
}

pub(super) fn explore_panel_meta(call: &ToolCall) -> String {
    tool_path_arg(call)
        .map(display_path)
        .unwrap_or_else(|| "workspace".to_string())
}

pub(super) fn explore_body(call: &ToolCall) -> String {
    exploration_summary(call)
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
    for line in diff.lines() {
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        }
        if let Some((old_start, new_start)) = parse_hunk_header(line) {
            old_line = old_start;
            new_line = new_start;
            continue;
        }
        let Some(marker) = line.chars().next() else {
            continue;
        };
        let code = line.get(marker.len_utf8()..).unwrap_or_default();
        let (old, new, marker_text, style, bg) = match marker {
            '-' => {
                let row = (Some(old_line), None, "-", err_style(), Some(DIFF_DEL_BG));
                old_line += 1;
                row
            }
            '+' => {
                let row = (None, Some(new_line), "+", ok_style(), Some(DIFF_ADD_BG));
                new_line += 1;
                row
            }
            ' ' => {
                let row = (Some(old_line), Some(new_line), " ", panel_style(), None);
                old_line += 1;
                new_line += 1;
                row
            }
            _ => continue,
        };
        let row = format_diff_table_row(old, new, marker_text, code);
        out.push(TranscriptRow::chrome_with_text(
            ChromeRow::Body {
                line: Line::from(Span::styled(row.clone(), style)),
                bg,
            },
            row,
            style,
        ));
    }
    out
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

fn format_diff_table_row(
    old: Option<usize>,
    new: Option<usize>,
    marker: &str,
    code: &str,
) -> String {
    let old = old.map_or_else(String::new, |line| line.to_string());
    let new = new.map_or_else(String::new, |line| line.to_string());
    format!("{old:>4}   {new:>7}  {marker}  |  {code}")
}
