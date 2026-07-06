//! Transcript row representation and row-level rendering helpers.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::component::Component;
use super::panel::{
    apply_width_bg, footer_rule_line, inset_rule_line, panel_body_content_width, panel_body_line,
    panel_body_lines, panel_footer_content_width, panel_footer_line, panel_header_line,
    rail_header_line,
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
    /// Whether this row is canonical transcript content that `/find` should
    /// search. `false` marks fold affordance/control chrome (the `ctrl+o to
    /// expand`/`collapse` hints) so searching for their literal text does not
    /// match hidden UI and auto-expand panels for non-content. Defaults to
    /// `true`; only the fold-hint builders clear it.
    pub(super) searchable: bool,
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
            searchable: true,
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
            searchable: true,
        }
    }

    /// Push this row's physical (wrapped) lines into `out`. Shared by the
    /// [`Component::render_into`] override; kept as an inherent method so other
    /// `ui::tui` modules can append rows without a trait import.
    pub(super) fn render_rows(&self, width: usize, out: &mut Vec<Line<'static>>) {
        if let Some(chrome) = &self.chrome {
            // Block boundary markers are structural only: a frameless block
            // draws no top or bottom border row.
            if matches!(chrome, ChromeRow::BlockStart | ChromeRow::BlockEnd) {
                return;
            }
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
    /// Start-of-block marker. Renders nothing (frameless blocks have no top
    /// border); bounds the block for trim/replace logic and resets fold state.
    BlockStart,
    /// The frameless block header: `▾ TOOL  meta … elapsed`. The right edge
    /// carries ONLY the elapsed time; state lives in the footer.
    Header {
        expanded: bool,
        title: &'static str,
        meta: String,
        elapsed: String,
    },
    /// The hairline rule that opens the always-visible block footer.
    FooterRule,
    /// The block footer row: state label (+ family extras) left, right-bound
    /// dim diagnostics. Always visible, expanded or collapsed. `diag_call` tags
    /// the tool call whose footer this is, so a following provider turn can
    /// locate and patch the right-bound `↑/cache/ctx` diagnostics in place
    /// (forward attribution); `None` for non-tool footers (task diff, denials).
    Footer {
        left: Line<'static>,
        right: String,
        diag_call: Option<String>,
    },
    /// End-of-block marker. Renders nothing; the footer is the last visible
    /// row of a block.
    BlockEnd,
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
            // Structural markers; render_rows short-circuits before this.
            ChromeRow::BlockStart | ChromeRow::BlockEnd => Line::default(),
            ChromeRow::Header {
                expanded,
                title,
                meta,
                elapsed,
            } => panel_header_line(width, *expanded, title, meta, elapsed),
            ChromeRow::FooterRule => footer_rule_line(width),
            // The state label (and family extras) always win the footer row:
            // when the optional diagnostics cluster does not fit, it is
            // dropped rather than displacing the left side.
            ChromeRow::Footer { left, right, .. } => {
                let content_width = panel_footer_content_width(width);
                let left_w = display_width(&line_text(left));
                let right_w = display_width(right);
                let line = if right.is_empty() || left_w + 1 + right_w > content_width {
                    left.clone()
                } else {
                    right_aligned_line(left.clone(), right, dim_style(), content_width)
                };
                panel_footer_line(width, line)
            }
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

/// Which side survives when `left` and `right` cannot both fit in `width`.
pub(super) enum Overflow {
    /// Drop `left` entirely, keep `right` hugging the right edge (tool-panel
    /// fold affordances / metadata).
    DropLeft,
    /// Keep `left` in full, drop `right`. Retained as the symmetric `right_align`
    /// overflow policy (exercised by its unit tests); the transcript fold hint
    /// that used it went away when THINKING adopted the tool-block `▾`/`▸`
    /// disclosure (no `… N more paragraphs` row).
    #[allow(dead_code)]
    KeepLeft,
}

/// Pad `left` so `right` hugs the right edge of a `width`-column field, keeping
/// at least `min_gap` blank columns between them. When they cannot both fit,
/// `overflow` decides which side survives. The single string right-align helper,
/// unifying the former `tool_render::right_align_hint` (`DropLeft` with a
/// one-column min gap) and `transcript::right_align_pair` (`KeepLeft` with a
/// two-column min gap) -- which did not render identically, differing in exactly
/// this overflow policy.
pub(super) fn right_align(
    left: &str,
    right: &str,
    width: usize,
    min_gap: usize,
    overflow: Overflow,
) -> String {
    let left_w = display_width(left);
    let right_w = display_width(right);
    if left_w + min_gap + right_w <= width {
        let gap = (width - left_w - right_w).max(min_gap);
        return format!("{left}{}{right}", " ".repeat(gap));
    }
    match overflow {
        Overflow::DropLeft => {
            format!("{}{right}", " ".repeat(width.saturating_sub(right_w)))
        }
        Overflow::KeepLeft => left.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_align_pads_right_when_both_fit() {
        // Both sides fit: `right` hugs the edge, `left` stays put.
        let out = right_align("left", "right", 20, 1, Overflow::DropLeft);
        assert_eq!(display_width(&out), 20);
        assert!(out.starts_with("left"));
        assert!(out.ends_with("right"));
        // At least `min_gap` blank columns separate them.
        assert!(out.contains("left") && out.contains("  "));
    }

    #[test]
    fn right_align_drop_left_policy_keeps_right_on_overflow() {
        // Too narrow for both: DropLeft discards `left`, right-aligns `right`.
        let out = right_align("a-very-long-left", "hint", 8, 1, Overflow::DropLeft);
        assert_eq!(out, "    hint");
        assert!(!out.contains("long"));
    }

    #[test]
    fn right_align_keep_left_policy_drops_right_on_overflow() {
        // Too narrow for both: KeepLeft returns `left` unchanged, drops `right`.
        let out = right_align("a-very-long-left", "hint", 8, 2, Overflow::KeepLeft);
        assert_eq!(out, "a-very-long-left");
    }

    #[test]
    fn right_align_min_gap_governs_the_overflow_threshold() {
        // left=5, right=3, width=9. DropLeft (min_gap 1): 5+1+3=9 <= 9 -> fits.
        assert_eq!(
            right_align("lllll", "rrr", 9, 1, Overflow::DropLeft),
            "lllll rrr"
        );
        // KeepLeft (min_gap 2): 5+2+3=10 > 9 -> overflow, keep left only.
        assert_eq!(
            right_align("lllll", "rrr", 9, 2, Overflow::KeepLeft),
            "lllll"
        );
    }
}
