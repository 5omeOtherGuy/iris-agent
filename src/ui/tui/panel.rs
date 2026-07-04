//! Frameless tool-block chrome (header · hanging body · hairline footer),
//! block metadata, and edit-table rendering.

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
    BOX_X_PADDING, PANEL_BODY_INDENT, PANEL_FOOTER_INDENT, TEXT_COLUMN_X_PADDING, diff_add_bg,
    diff_del_bg, dim_style, err_style, ok_style, panel_style, prompt_style,
};
use crate::ui::{is_diff_file_header, symbols};

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

pub(super) fn panel_body_content_width(width: usize) -> usize {
    panel_width(width).saturating_sub(PANEL_BODY_INDENT).max(1)
}

pub(super) fn panel_footer_content_width(width: usize) -> usize {
    panel_width(width)
        .saturating_sub(PANEL_FOOTER_INDENT)
        .max(1)
}

/// The frameless block header (`FramelessHeader`):
/// `▾ TOOL  meta …spacer… elapsed` — disclosure glyph (muted), bold uppercase
/// family label, muted meta truncating with `…`, and the elapsed time as the
/// only right-edge content. No state symbol, no frame.
pub(super) fn panel_header_line(
    width: usize,
    expanded: bool,
    title: &'static str,
    meta: &str,
    elapsed: &str,
) -> Line<'static> {
    let inner_width = panel_width(width);
    let arrow = if expanded {
        symbols::EXPANDED
    } else {
        symbols::COLLAPSED
    };
    let mut left = vec![
        Span::styled(format!("{arrow} "), dim_style()),
        // The tool family is the block's identity. With no type-scale axis,
        // weight is the hierarchy lever (DESIGN-LANGUAGE: label = bold).
        Span::styled(
            title.to_string(),
            panel_style().add_modifier(Modifier::BOLD),
        ),
    ];
    let meta = strip_ansi_for_text(meta);
    if !meta.is_empty() {
        left.push(Span::styled(format!("  {meta}"), dim_style()));
    }
    let elapsed = strip_ansi_for_text(elapsed);
    let right = if elapsed.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(elapsed, dim_style())]
    };
    let right = take_spans_to_width(right, inner_width / 2);
    let right_width = spans_width(&right);
    let reserve = if right_width == 0 { 0 } else { right_width + 1 };
    let left = take_spans_to_width(left, inner_width.saturating_sub(reserve));
    let left_width = spans_width(&left);
    let spacer = inner_width
        .saturating_sub(left_width)
        .saturating_sub(right_width);
    let outer = panel_outer_padding(width);
    let mut spans = vec![Span::raw(" ".repeat(outer))];
    spans.extend(left);
    spans.push(Span::raw(" ".repeat(spacer)));
    spans.extend(right);
    let mut line = Line::from(spans);
    truncate_line(&mut line, width.max(1));
    line
}

/// The hairline rule that opens every block footer: starts at the footer indent
/// (one cell left of the body) and runs to the block's right edge. The one rule
/// the frameless design keeps.
pub(super) fn footer_rule_line(width: usize) -> Line<'static> {
    let outer = panel_outer_padding(width);
    let indent = PANEL_FOOTER_INDENT.min(panel_width(width));
    let rule_width = panel_width(width).saturating_sub(indent).max(1);
    let mut line = Line::from(vec![
        Span::raw(" ".repeat(outer + indent)),
        Span::styled("─".repeat(rule_width), dim_style()),
    ]);
    truncate_line(&mut line, width.max(1));
    line
}

/// A footer content row (state label + extras): sits at the footer indent, one
/// cell left of the body, its right edge on the block's right rail.
pub(super) fn panel_footer_line(width: usize, mut line: Line<'static>) -> Line<'static> {
    let footer_width = panel_footer_content_width(width);
    truncate_line(&mut line, footer_width);
    let outer = panel_outer_padding(width);
    let mut spans = vec![Span::raw(" ".repeat(outer + PANEL_FOOTER_INDENT))];
    spans.extend(line.spans);
    let mut line = Line::from(spans);
    truncate_line(&mut line, width.max(1));
    line
}

pub(super) fn panel_body_line(
    width: usize,
    mut line: Line<'static>,
    bg: Option<Color>,
) -> Line<'static> {
    let body_width = panel_body_content_width(width);
    truncate_line(&mut line, body_width);
    if let Some(bg) = bg {
        apply_width_bg(&mut line, bg, body_width);
    }
    let outer = panel_outer_padding(width);
    let mut spans = vec![Span::raw(" ".repeat(outer + PANEL_BODY_INDENT))];
    spans.extend(line.spans);
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
    let body_width = panel_body_content_width(width);
    let mut wrapped = Vec::new();
    push_wrapped_line(&line, body_width, None, &mut wrapped);
    for physical in wrapped {
        out.push(panel_body_line(width, physical, bg));
    }
}

/// A reasoning-rail header: `▾ THINKING` (expanded) / `▸ THINKING` (collapsed),
/// muted, bold label, indented to the transcript text column, with optional
/// right-aligned telemetry (`↓2.4k 12s`). No box — reasoning is recessive; the
/// `┊` rail on its body rows is the only chrome it gets (ThinkingBlock). A
/// non-foldable (short) block drops the disclosure arrow.
pub(super) fn rail_header_line(
    width: usize,
    expanded: bool,
    foldable: bool,
    label: &str,
    right: &str,
) -> Line<'static> {
    let left = if foldable {
        let arrow = if expanded {
            symbols::EXPANDED
        } else {
            symbols::COLLAPSED
        };
        format!("{arrow} {label}")
    } else {
        label.to_string()
    };
    let mut spans = vec![Span::styled(
        left.clone(),
        dim_style().add_modifier(Modifier::BOLD),
    )];
    if !right.is_empty() {
        let content_width = width
            .saturating_sub(TEXT_COLUMN_X_PADDING.saturating_mul(2))
            .max(1);
        let gap = content_width
            .saturating_sub(display_width(&left))
            .saturating_sub(display_width(right));
        if gap >= 1 {
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(right.to_string(), dim_style()));
        }
    }
    let mut line = Line::from(spans);
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
    /// A pending mutation awaiting apply/approval (`◇ PREVIEW`, no elapsed).
    Preview,
}

impl PanelState {
    /// Footer state label — label only, no glyph (`◆`/`■` are NOT rendered in
    /// the frameless footer). The text carries the state in monochrome; color
    /// reinforces it.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Running => "RUNNING",
            Self::Done => "DONE",
            Self::Error => "ERROR",
            Self::Cancelled => "CANCELLED",
            Self::Preview => "PREVIEW",
        }
    }

    pub(super) fn dot_style(self) -> Style {
        match self {
            Self::Running => prompt_style(),
            Self::Done => ok_style(),
            Self::Error => err_style(),
            Self::Cancelled | Self::Preview => dim_style(),
        }
    }

    /// Footer label style: the state color plus BOLD, matching the uppercase
    /// tracked label treatment of the design's footer actor.
    pub(super) fn label_style(self) -> Style {
        self.dot_style().add_modifier(Modifier::BOLD)
    }

    pub(super) fn plain_prefix(self) -> &'static str {
        match self {
            Self::Running => "• Running",
            Self::Done => "• Ran",
            Self::Error => "✗ Ran",
            Self::Cancelled => "• Cancelled",
            Self::Preview => "• Preview",
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
        if is_diff_file_header(&lines, i) {
            i += 2;
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
                    if is_diff_header_line(&lines, i) || !next.starts_with('-') {
                        break;
                    }
                    removed.push(next.get('-'.len_utf8()..).unwrap_or_default());
                    i += 1;
                }
                let mut added: Vec<&str> = Vec::new();
                while let Some(next) = lines.get(i) {
                    if is_diff_header_line(&lines, i) || !next.starts_with('+') {
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
                        Some(diff_del_bg()),
                    ));
                    old_line += 1;
                    out.push(diff_span_row(
                        None,
                        Some(new_line),
                        symbols::ADDED,
                        added[0],
                        new_spans,
                        ok_style(),
                        Some(diff_add_bg()),
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
                            Some(diff_del_bg()),
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
                            Some(diff_add_bg()),
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
                    Some(diff_add_bg()),
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
    let lines: Vec<&str> = diff.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if is_diff_header_line(&lines, i) {
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

/// One footer metadata field: a styled span run plus its plain-text mirror.
/// Fields are joined by [`join_meta_fields`]; a field is never split by `┊`
/// internally (`+n −n`, `↑… ↓…`, and `EXIT n` are each ONE field).
pub(super) struct FooterField {
    pub(super) spans: Vec<Span<'static>>,
    pub(super) plain: String,
}

impl FooterField {
    pub(super) fn styled(text: impl Into<String>, style: Style) -> Self {
        let text = text.into();
        Self {
            spans: vec![Span::styled(text.clone(), style)],
            plain: text,
        }
    }
}

/// The `┊` law (DESIGN-LANGUAGE §6), joined programmatically: the soft
/// separator sits only BETWEEN sibling metadata fields, one space each side —
/// never leading, never trailing, never doubled. Empty fields are filtered so
/// a missing field can never leave a dangling `┊` (the `MetaFields` joiner).
pub(super) fn join_meta_fields(fields: Vec<FooterField>) -> (Vec<Span<'static>>, String) {
    let mut spans = Vec::new();
    let mut plain = String::new();
    for field in fields.into_iter().filter(|field| !field.plain.is_empty()) {
        if !plain.is_empty() {
            spans.push(Span::styled(format!(" {} ", symbols::SEP), dim_style()));
            plain.push_str(&format!(" {} ", symbols::SEP));
        }
        spans.extend(field.spans);
        plain.push_str(&field.plain);
    }
    (spans, plain)
}

/// Token diagnostics for the block footer, right-bound:
/// `↑<sent> ↓<received> ┊ cache <n> ┊ ctx <Δ%>`. Forward-attributed and
/// round-level (the finest honest granularity): `↓received` is the output the
/// *proposing* turn generated (known immediately); `↑sent` (fresh non-cached
/// input), `cache` (prompt-cache reads), and `ctx` (context growth vs the
/// proposing turn) come from the *following* turn that ingests the tool
/// results, patched onto the footer when that turn's usage arrives. Tool calls
/// proposed by the same turn share the input-side numbers. All fields are
/// optional, preformatted strings (`"1.4k"`, `"+0.9%"`); a field is rendered
/// only when the runtime measured it — never a fabricated per-call split.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToolDiag {
    pub(crate) sent: Option<String>,
    pub(crate) received: Option<String>,
    pub(crate) cache: Option<String>,
    pub(crate) ctx: Option<String>,
}

impl ToolDiag {
    /// Render the diagnostics cluster, or `None` when no field is present.
    /// `↑sent ↓received` is one field; `cache` and `ctx` follow, `┊`-joined
    /// via [`join_meta_fields`] so omissions never leave a dangling separator.
    pub(super) fn render(&self) -> Option<String> {
        let updown = match (&self.sent, &self.received) {
            (Some(sent), Some(received)) => Some(format!("↑{sent} ↓{received}")),
            (Some(sent), None) => Some(format!("↑{sent}")),
            (None, Some(received)) => Some(format!("↓{received}")),
            (None, None) => None,
        };
        let fields: Vec<FooterField> = [
            updown,
            self.cache.as_ref().map(|cache| format!("cache {cache}")),
            self.ctx.as_ref().map(|ctx| format!("ctx {ctx}")),
        ]
        .into_iter()
        .flatten()
        .map(|text| FooterField::styled(text, dim_style()))
        .collect();
        if fields.is_empty() {
            return None;
        }
        let (_, plain) = join_meta_fields(fields);
        Some(plain)
    }
}

/// The EDIT footer extras: the `+n −n` counts as ONE field (additions green,
/// removals red, 1ch apart), then the muted note (`new file`) as a sibling
/// field. Unicode minus, never ASCII `-`.
pub(super) fn edit_footer_extras(
    added: usize,
    removed: usize,
    note: Option<&str>,
) -> Vec<FooterField> {
    let mut fields = Vec::new();
    if added + removed > 0 {
        fields.push(FooterField {
            spans: vec![
                Span::styled(format!("{}{added}", symbols::ADDED), ok_style()),
                Span::raw(" "),
                Span::styled(format!("{}{removed}", symbols::REMOVED), err_style()),
            ],
            plain: format!("{}{added} {}{removed}", symbols::ADDED, symbols::REMOVED),
        });
    }
    if let Some(note) = note {
        fields.push(FooterField::styled(note.to_string(), dim_style()));
    }
    fields
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

fn is_diff_header_line(lines: &[&str], i: usize) -> bool {
    is_diff_file_header(lines, i)
        || i.checked_sub(1)
            .is_some_and(|prev| is_diff_file_header(lines, prev))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_rows_count_removed_content_that_looks_like_file_header() {
        let diff = "--- a/query.sql\n+++ b/query.sql\n@@ -1,2 +1 @@\n--- comment\n keep\n";

        let rows = diff_table_rows(diff);
        let rendered = rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("-- comment"), "{rendered}");
        assert_eq!(diff_counts(diff), (0, 1));
    }

    #[test]
    fn meta_fields_joiner_never_leaves_dangling_separators() {
        // The `┊` law: only BETWEEN sibling fields, one space each side,
        // never leading/trailing/doubled; empty fields are filtered.
        let fields = |texts: &[&str]| {
            texts
                .iter()
                .map(|t| FooterField::styled(t.to_string(), dim_style()))
                .collect::<Vec<_>>()
        };
        let (_, plain) = join_meta_fields(fields(&["EXIT 0", "142 passed"]));
        assert_eq!(plain, "EXIT 0 ┊ 142 passed");
        let (_, plain) = join_meta_fields(fields(&["EXIT 0", "", "142 passed"]));
        assert_eq!(plain, "EXIT 0 ┊ 142 passed");
        let (_, plain) = join_meta_fields(fields(&["", "only"]));
        assert_eq!(plain, "only");
        let (spans, plain) = join_meta_fields(Vec::new());
        assert!(spans.is_empty() && plain.is_empty());
    }

    #[test]
    fn tool_diag_renders_optional_fields_without_dangling_separators() {
        let full = ToolDiag {
            sent: Some("1.4k".to_string()),
            received: Some("38".to_string()),
            cache: Some("16.8k".to_string()),
            ctx: Some("+0.9%".to_string()),
        };
        assert_eq!(
            full.render().as_deref(),
            Some("↑1.4k ↓38 ┊ cache 16.8k ┊ ctx +0.9%")
        );
        // `↑sent ↓received` is ONE field; omitting parts never leaves a `┊`
        // at a cluster edge or inside the field.
        let partial = ToolDiag {
            sent: None,
            received: Some("38".to_string()),
            cache: None,
            ctx: Some("+0.9%".to_string()),
        };
        assert_eq!(partial.render().as_deref(), Some("↓38 ┊ ctx +0.9%"));
        let sent_only = ToolDiag {
            sent: Some("1.4k".to_string()),
            ..ToolDiag::default()
        };
        assert_eq!(sent_only.render().as_deref(), Some("↑1.4k"));
        assert_eq!(ToolDiag::default().render(), None);
    }

    #[test]
    fn edit_footer_extras_join_counts_as_one_field_with_note() {
        let (_, plain) = join_meta_fields(edit_footer_extras(1, 1, Some("new file")));
        assert_eq!(plain, "+1 −1 ┊ new file");
        let (_, plain) = join_meta_fields(edit_footer_extras(2, 0, None));
        assert_eq!(plain, "+2 −0");
        let (spans, plain) = join_meta_fields(edit_footer_extras(0, 0, None));
        assert!(spans.is_empty() && plain.is_empty());
    }

    #[test]
    fn footer_rule_is_a_hairline_from_the_footer_indent_to_the_right_edge() {
        let line = footer_rule_line(80);
        let text = line_text(&line);
        assert!(text.starts_with("    ─"), "{text:?}");
        assert_eq!(display_width(&text), 78, "{text:?}");
        assert!(text.trim_start().chars().all(|c| c == '─'), "{text:?}");
    }

    #[test]
    fn header_carries_disclosure_label_meta_and_elapsed_only() {
        let line = panel_header_line(80, true, "EXPLORE", "src/context", "0.0s");
        let text = line_text(&line);
        assert!(text.starts_with("  ▾ EXPLORE  src/context"), "{text:?}");
        assert!(text.trim_end().ends_with("0.0s"), "{text:?}");
        assert_eq!(display_width(text.trim_end()), 78, "{text:?}");
        let collapsed = line_text(&panel_header_line(
            80,
            false,
            "EXPLORE",
            "src/context",
            "0.0s",
        ));
        assert!(collapsed.starts_with("  ▸ EXPLORE"), "{collapsed:?}");
    }
}
