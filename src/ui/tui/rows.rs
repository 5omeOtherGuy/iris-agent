//! Transcript row representation and row-level rendering helpers.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::component::Component;
use super::panel::{
    apply_width_bg, inset_rule_line, panel_body_content_width, panel_body_line, panel_body_lines,
    panel_header_line, panel_rule_line, rail_header_line,
};
use super::wrap::{
    display_width, line_text, pad_line_left, pad_line_right, push_wrapped_line,
    push_wrapped_line_wordwise, push_wrapped_line_wordwise_with_prefix, push_wrapped_row,
    push_wrapped_row_with_prefix, truncate_line, truncate_to_width,
};
use super::{BOX_X_PADDING, TEXT_COLUMN_X_PADDING, TEXT_X_PADDING, dim_style};

/// Per-row visibility within a foldable tool-output panel. `Always` rows show
/// in both states; `WhenCollapsed`/`WhenExpanded` rows show only in the
/// preview (capped) or fully-revealed state respectively. The enclosing panel
/// header's `expanded` flag selects which set renders.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FoldVis {
    Always,
    WhenCollapsed,
    WhenExpanded,
}

/// One styled logical transcript row. Most rows are plain text + style; ANSI
/// tool output stores a parsed ratatui line so the escape styling survives.
#[derive(Clone)]
pub(super) struct TranscriptRow {
    pub(super) text: String,
    pub(super) style: Style,
    pub(super) continuation_prefix: Option<&'static str>,
    pub(super) line: Option<Line<'static>>,
    /// Fold-state visibility for collapsible tool-output bodies.
    pub(super) fold: FoldVis,
    /// Word-aware (space-breaking, URL-safe) wrap for this row's styled `line`
    /// instead of the default ANSI char-hard-wrap. Set for Markdown prose; the
    /// gutter/ANSI tool-output rows keep char-wrap so their leading-space
    /// prefixes are never collapsed.
    pub(super) word_wrap: bool,
    /// Full-width background fill applied to every physical row this logical row
    /// wraps to. `None` for ordinary rows; panel bodies use this for diff rows.
    pub(super) background: Option<Color>,
    /// A horizontal-rule row (Codex's turn separator). When set, `text` is the
    /// optional centered label and the row renders as `─ label ─────` to width.
    pub(super) hrule: bool,
    pub(super) chrome: Option<ChromeRow>,
}

impl TranscriptRow {
    pub(super) fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            continuation_prefix: None,
            line: None,
            fold: FoldVis::Always,
            word_wrap: false,
            background: None,
            hrule: false,
            chrome: None,
        }
    }

    /// Tag this row with a fold-state visibility (builder style).
    pub(super) fn with_fold(mut self, fold: FoldVis) -> Self {
        self.fold = fold;
        self
    }

    pub(super) fn chrome(chrome: ChromeRow) -> Self {
        Self::chrome_with_text(chrome, String::new(), Style::default())
    }

    pub(super) fn chrome_with_text(chrome: ChromeRow, text: String, style: Style) -> Self {
        Self {
            text,
            style,
            continuation_prefix: None,
            line: None,
            fold: FoldVis::Always,
            word_wrap: false,
            background: None,
            hrule: false,
            chrome: Some(chrome),
        }
    }

    /// Push this row's physical (wrapped) lines into `out`. Shared by the
    /// [`Component::render_into`] override; kept as an inherent method so other
    /// `ui::tui` modules can append rows without a trait import.
    pub(super) fn render_rows(&self, width: usize, out: &mut Vec<Line<'static>>) {
        if let Some(chrome) = &self.chrome {
            if let ChromeRow::Body { line, bg } = chrome {
                panel_body_lines(width, line.clone(), *bg, out);
                return;
            }
            if let ChromeRow::BodyRight {
                left,
                right,
                right_style,
                bg,
            } = chrome
            {
                let content_width = panel_body_content_width(width);
                let left_width = display_width(&line_text(left));
                let right_width = display_width(right);
                if !right.is_empty() && left_width + 1 + right_width > content_width {
                    panel_body_lines(width, left.clone(), *bg, out);
                    out.push(panel_body_line(
                        width,
                        right_aligned_line(Line::default(), right, *right_style, content_width),
                        *bg,
                    ));
                    return;
                }
            }
            out.push(chrome.render(width));
            return;
        }
        if self.hrule {
            out.push(inset_rule_line(width, &self.text));
            return;
        }
        let boxed = self.background.is_some();
        let box_width = if boxed {
            width.saturating_sub(BOX_X_PADDING * 2).max(1)
        } else {
            width
        };
        let content_padding = row_text_padding(self);
        let render_width = box_width
            .saturating_sub(content_padding.saturating_mul(2))
            .max(1);
        let start = out.len();
        match &self.line {
            Some(line) if self.word_wrap => {
                if let Some(prefix) = self.continuation_prefix {
                    push_wrapped_line_wordwise_with_prefix(line, render_width, prefix, out);
                } else {
                    push_wrapped_line_wordwise(line, render_width, out);
                }
            }
            Some(line) => push_wrapped_line(line, render_width, self.continuation_prefix, out),
            None if self.word_wrap => {
                if let Some(prefix) = self.continuation_prefix {
                    push_wrapped_row_with_prefix(&self.text, self.style, render_width, prefix, out);
                } else {
                    push_wrapped_row(&self.text, self.style, render_width, None, out);
                }
            }
            None => push_wrapped_row(
                &self.text,
                self.style,
                render_width,
                self.continuation_prefix,
                out,
            ),
        }
        if content_padding > 0 {
            for physical in &mut out[start..] {
                pad_line_left(physical, content_padding);
            }
        }
        if let Some(bg) = self.background {
            for physical in &mut out[start..] {
                apply_width_bg(physical, bg, box_width);
                pad_line_left(physical, BOX_X_PADDING);
                pad_line_right(physical, BOX_X_PADDING);
            }
        }
    }
}

impl Component for TranscriptRow {
    fn render(&self, width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        self.render_rows(width, &mut out);
        out
    }

    /// Append directly so the transcript's borrowed `composite` over thousands
    /// of rows allocates no intermediate per-row `Vec`.
    fn render_into(&self, width: usize, out: &mut Vec<Line<'static>>) {
        self.render_rows(width, out);
    }
}

#[derive(Clone)]
pub(super) enum ChromeRow {
    Top,
    Header {
        expanded: bool,
        title: &'static str,
        meta: String,
        right: Vec<(String, Style)>,
    },
    Separator,
    Bottom,
    Body {
        line: Line<'static>,
        bg: Option<Color>,
    },
    BodyRight {
        left: Line<'static>,
        right: String,
        right_style: Style,
        bg: Option<Color>,
    },
    BodyRule {
        prefix: String,
        rule: char,
        style: Style,
        bg: Option<Color>,
    },
    Notice {
        glyph: String,
        glyph_style: Style,
        message: String,
        hint: String,
    },
    /// A reasoning-rail header — a chromeless fold anchor. Carries the same
    /// `expanded` flag the fold machinery reads (so `ctrl+o` and the visibility
    /// pass treat it like a panel `Header`), but renders as a muted `┊ ▾ THINKING`
    /// rail rather than a box, because reasoning is recessive (ThinkingBlock).
    RailHeader {
        expanded: bool,
        label: String,
        /// Right-aligned dim telemetry (`↓2.4k 12s`); empty for none.
        right: String,
        /// Whether the block folds (drives the `▾`/`▸` disclosure arrow; a
        /// short trace shown whole has no arrow and ignores ctrl+o).
        foldable: bool,
    },
    /// The end marker of a reasoning rail — the rail analogue of `Bottom`. Bounds
    /// the block for `panel_end_from`/the visibility reset and renders as a single
    /// blank line of breathing room (no border).
    RailEnd,
}

impl ChromeRow {
    pub(super) fn render(&self, width: usize) -> Line<'static> {
        match self {
            ChromeRow::Top => panel_rule_line(width, '┌', '┐'),
            ChromeRow::Header {
                expanded,
                title,
                meta,
                right,
            } => panel_header_line(width, *expanded, title, meta, right),
            ChromeRow::Separator => panel_rule_line(width, '├', '┤'),
            ChromeRow::Bottom => panel_rule_line(width, '└', '┘'),
            ChromeRow::Body { line, bg } => panel_body_line(width, line.clone(), *bg),
            ChromeRow::BodyRight {
                left,
                right,
                right_style,
                bg,
            } => panel_body_line(
                width,
                right_aligned_line(
                    left.clone(),
                    right,
                    *right_style,
                    panel_body_content_width(width),
                ),
                *bg,
            ),
            ChromeRow::BodyRule {
                prefix,
                rule,
                style,
                bg,
            } => panel_body_line(
                width,
                body_rule_line(prefix, *rule, *style, panel_body_content_width(width)),
                *bg,
            ),
            ChromeRow::Notice {
                glyph,
                glyph_style,
                message,
                hint,
            } => notice_line(width, glyph, *glyph_style, message, hint),
            ChromeRow::RailHeader {
                expanded,
                label,
                right,
                foldable,
            } => rail_header_line(width, *expanded, *foldable, label, right),
            ChromeRow::RailEnd => Line::default(),
        }
    }
}

fn right_aligned_line(
    left: Line<'static>,
    right: &str,
    right_style: Style,
    width: usize,
) -> Line<'static> {
    let right_w = display_width(right);
    let left_w = display_width(&line_text(&left));
    if right.is_empty() {
        return left;
    }
    if left_w + 1 + right_w > width {
        return Line::from(vec![
            Span::raw(" ".repeat(width.saturating_sub(right_w))),
            Span::styled(right.to_string(), right_style),
        ]);
    }
    let mut spans = left.spans;
    spans.push(Span::raw(" ".repeat(
        width.saturating_sub(left_w).saturating_sub(right_w).max(1),
    )));
    spans.push(Span::styled(right.to_string(), right_style));
    Line::from(spans)
}

fn body_rule_line(prefix: &str, rule: char, style: Style, width: usize) -> Line<'static> {
    let prefix_w = display_width(prefix);
    let fill = width.saturating_sub(prefix_w).max(1);
    Line::from(Span::styled(
        format!("{prefix}{}", rule.to_string().repeat(fill)),
        style,
    ))
}

fn notice_line(
    width: usize,
    glyph: &str,
    glyph_style: Style,
    message: &str,
    hint: &str,
) -> Line<'static> {
    let left = format!("{glyph} {message}");
    let mut spans = vec![
        Span::styled(format!("{glyph} "), glyph_style),
        Span::styled(message.to_string(), dim_style()),
    ];
    if !hint.is_empty() {
        let content_width = width
            .saturating_sub(TEXT_COLUMN_X_PADDING.saturating_mul(2))
            .max(1);
        let left_w = display_width(&left);
        let hint_w = display_width(hint);
        if left_w + 2 + hint_w <= content_width {
            spans.push(Span::raw(" ".repeat(content_width - left_w - hint_w)));
            spans.push(Span::styled(hint.to_string(), dim_style()));
        }
    }
    let mut line = Line::from(spans);
    pad_line_left(&mut line, TEXT_COLUMN_X_PADDING);
    truncate_line(&mut line, width.max(1));
    line
}

const TURN_DIVIDER_LEADER_WIDTH: usize = TEXT_COLUMN_X_PADDING + 4 - BOX_X_PADDING;

/// Build a dim full-width horizontal rule, optionally wrapping a quiet turn
/// summary label (`────── 7.6s ┊ ↑18.2k ↓846 ───────`).
pub(super) fn hrule_line(label: &str, width: usize) -> Line<'static> {
    let width = width.max(1);
    if label.is_empty() {
        return Line::from(Span::styled("\u{2500}".repeat(width), dim_style()));
    }
    let leader = "\u{2500}".repeat(TURN_DIVIDER_LEADER_WIDTH);
    let text = truncate_to_width(&format!("{leader} {label} \u{2500}"), width);
    let fill = width.saturating_sub(display_width(&text));
    Line::from(Span::styled(
        format!("{text}{}", "\u{2500}".repeat(fill)),
        dim_style(),
    ))
}

/// A block-separator row: the empty plain row `push_blank` inserts between
/// top-level blocks. Distinguished from a Markdown-internal blank line (which
/// carries a styled `line`) and from a turn-rule row so block grouping remains
/// stable while the terminal surface replays from Iris state.
pub(super) fn is_separator_row(row: &TranscriptRow) -> bool {
    !row.hrule
        && row.chrome.is_none()
        && row.text.is_empty()
        && row.line.is_none()
        && row.background.is_none()
}

pub(super) fn row_text_padding(row: &TranscriptRow) -> usize {
    if row.background.is_some() {
        usize::from(!row.text.is_empty()) * TEXT_X_PADDING
    } else if is_separator_row(row) {
        0
    } else {
        TEXT_COLUMN_X_PADDING
    }
}
