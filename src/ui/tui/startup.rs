//! The start page (Tier 3): the IrisMark LED strip, its silkscreen identity
//! row, and the keyboard-navigable launcher menu, shown when Iris launches
//! interactively with no task and no resume target -- before any transcript
//! exists. Rendered centered in the empty transcript area between the session
//! bar and the composer; the shared pane chrome (session bar on top, composer
//! on bottom) stays live around it.
//!
//! The logo IS an LED strip: one row of [`MARK_DOTS`] dots with a single lit
//! orange head sweeping back and forth (ping-pong, never wrapping) and a 2-dot
//! comet trail behind the travel direction. Under `IRIS_REDUCED_MOTION` the
//! mark holds a single static lit dot at the center, matching how the working
//! indicator freezes. The loop's spinner tick drives the animation; the head
//! advances one dot per [`MARK_ADVANCE_INTERVAL`]. The silkscreen row is
//! printed hardware, so it never animates.

use std::time::{Duration, Instant};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::palette;
use crate::ui::symbols;

use super::component::Component;
use super::wrap::truncate_line;
use super::{dim_style, prompt_style};

/// Number of LED cells in the IrisMark strip.
pub(crate) const MARK_DOTS: usize = 12;

/// Minimum wall-clock interval between head advances (~one dot per 130ms).
const MARK_ADVANCE_INTERVAL: Duration = Duration::from_millis(130);

/// Launcher menu width cap (marker + label + dotted leader + key hint).
const MENU_WIDTH: usize = 44;

/// The silkscreen wordmark: letter-spaced so the glyphs sit on the same open
/// grid as the LED cells above them. Printed, not lit — it never animates.
const WORDMARK: &str = "I R I S";

/// Compile-time crate version, the silkscreen rev on the faceplate.
const REV: &str = env!("CARGO_PKG_VERSION");

/// One launcher activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartAction {
    NewSession,
    ResumeSession,
    Tasks,
    Settings,
    Quit,
}

/// Launcher rows in display order: action label + key-chord hint. `Tasks` is a
/// first-class home entry (its own menu point) rather than a modal that pops on
/// startup: recoverable Iris tasks are reached from here, not forced in the face.
const MENU_ITEMS: [(StartAction, &str, &str); 5] = [
    (StartAction::NewSession, "New session", "ctrl-n"),
    (StartAction::ResumeSession, "Resume session", "ctrl-r"),
    (StartAction::Tasks, "Tasks", "ctrl-t"),
    (StartAction::Settings, "Settings", "ctrl-,"),
    (StartAction::Quit, "Quit", "ctrl-q"),
];

/// Start-page state: launcher selection plus the IrisMark ping-pong sweep.
pub(crate) struct StartPage {
    selected: usize,
    /// Head LED position, `0..MARK_DOTS`.
    head: usize,
    /// Sweep direction: `true` = rightward.
    forward: bool,
    /// Last head advance, so the ~130ms cadence is independent of the loop's
    /// tick rate.
    last_advance: Option<Instant>,
    /// `IRIS_REDUCED_MOTION`: hold a single static lit dot at the center.
    reduced_motion: bool,
    /// Recoverable Iris tasks in this workspace at launch (ADR-0031). Surfaced
    /// as a dim badge on the `Tasks` row instead of force-opening a picker, so
    /// the count is visible without a modal popping over the home menu.
    recoverable: usize,
    /// Whether ctrl+punctuation chords are receivable (kitty keyboard
    /// enhancement negotiated). Keymap honesty: `ctrl-,` is advertised only
    /// when the terminal can actually deliver it; the Settings row stays
    /// reachable by `↑`/`↓` + `↵` either way.
    punctuation_chords: bool,
}

impl StartPage {
    pub(crate) fn new(reduced_motion: bool, recoverable: usize, punctuation_chords: bool) -> Self {
        Self {
            selected: 0,
            head: 0,
            forward: true,
            last_advance: None,
            reduced_motion,
            recoverable,
            punctuation_chords,
        }
    }

    /// Apply the live reduced-motion preference. Entering reduced motion freezes
    /// the sweep at the center datum; leaving it resumes from the previous head.
    pub(crate) fn set_reduced_motion(&mut self, reduced_motion: bool) {
        self.reduced_motion = reduced_motion;
    }

    /// Move the selection up, wrapping around.
    pub(crate) fn up(&mut self) {
        self.selected = self.selected.checked_sub(1).unwrap_or(MENU_ITEMS.len() - 1);
    }

    /// Move the selection down, wrapping around.
    pub(crate) fn down(&mut self) {
        self.selected = (self.selected + 1) % MENU_ITEMS.len();
    }

    pub(crate) fn selected_action(&self) -> StartAction {
        MENU_ITEMS[self.selected].0
    }

    /// Advance the animation for one loop tick. Returns whether the mark moved
    /// (so the loop only redraws when something changed). Reduced motion never
    /// animates; the ~130ms cadence gates advances between ticks.
    pub(crate) fn tick(&mut self) -> bool {
        self.tick_at(Instant::now())
    }

    fn tick_at(&mut self, now: Instant) -> bool {
        if self.reduced_motion {
            return false;
        }
        let Some(last) = self.last_advance else {
            self.last_advance = Some(now);
            self.advance();
            return true;
        };
        let elapsed = now.duration_since(last);
        if elapsed < MARK_ADVANCE_INTERVAL {
            return false;
        }
        let intervals = elapsed.as_nanos() / MARK_ADVANCE_INTERVAL.as_nanos();
        let intervals = u32::try_from(intervals).unwrap_or(u32::MAX);
        self.last_advance = Some(last + MARK_ADVANCE_INTERVAL * intervals);
        self.advance();
        true
    }

    /// One ping-pong step: the head sweeps 0..=MARK_DOTS-1 and reverses at the
    /// ends, never wrapping.
    fn advance(&mut self) {
        if self.forward {
            if self.head + 1 >= MARK_DOTS {
                self.forward = false;
                self.head = self.head.saturating_sub(1);
            } else {
                self.head += 1;
            }
        } else if self.head == 0 {
            self.forward = true;
            self.head = 1.min(MARK_DOTS - 1);
        } else {
            self.head -= 1;
        }
    }

    #[cfg(test)]
    pub(crate) fn head(&self) -> usize {
        self.head
    }

    #[cfg(test)]
    pub(crate) fn advance_for_test(&mut self) {
        self.advance();
    }

    #[cfg(test)]
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    /// The silkscreen row directly under the strip: the letter-spaced wordmark
    /// anchored to the strip's left edge, the crate rev to its right edge —
    /// printed faceplate text, so wordmark is plain ink and the rev is dim.
    /// One row; never animated.
    fn silkscreen_spans(&self) -> Vec<Span<'static>> {
        let mark_width = MARK_DOTS * 2 - 1;
        let gap = mark_width
            .saturating_sub(WORDMARK.len())
            .saturating_sub(REV.len())
            .max(1);
        vec![
            Span::raw(WORDMARK),
            Span::raw(" ".repeat(gap)),
            Span::styled(REV, dim_style()),
        ]
    }

    /// The IrisMark row: [`MARK_DOTS`] single-spaced LED cells. The head is
    /// bright orange, trail-1 (one behind the travel direction) plain orange,
    /// trail-2 dimmest; every other cell is a dim empty dot. Reduced motion
    /// holds one static lit dot at the center.
    fn mark_spans(&self) -> Vec<Span<'static>> {
        let head_style = prompt_style().add_modifier(Modifier::BOLD);
        let trail_1_style = prompt_style();
        // The comet fades in its own hue: trail-2 is a dim orange, not grey, so
        // it stays part of the orange sweep and never collapses into the muted
        // `○` empty dots behind it.
        let trail_2_style = prompt_style().add_modifier(Modifier::DIM);
        let (head, trail_1, trail_2) = if self.reduced_motion {
            (Some(MARK_DOTS / 2), None, None)
        } else {
            let behind = |steps: usize| {
                if self.forward {
                    self.head.checked_sub(steps)
                } else {
                    let pos = self.head + steps;
                    (pos < MARK_DOTS).then_some(pos)
                }
            };
            (Some(self.head), behind(1), behind(2))
        };
        let mut spans = Vec::with_capacity(MARK_DOTS * 2 - 1);
        for cell in 0..MARK_DOTS {
            if cell > 0 {
                spans.push(Span::raw(" "));
            }
            let span = if Some(cell) == head {
                Span::styled(symbols::RUNNING.to_string(), head_style)
            } else if Some(cell) == trail_1 {
                Span::styled(symbols::RUNNING.to_string(), trail_1_style)
            } else if Some(cell) == trail_2 {
                Span::styled(symbols::RUNNING.to_string(), trail_2_style)
            } else {
                Span::styled(symbols::EMPTY.to_string(), dim_style())
            };
            spans.push(span);
        }
        spans
    }

    /// One launcher row in the house picker idiom: a 1-col marker (`◉` orange
    /// on the selected row), the action label (bold when selected), a dim
    /// dotted leader, and the right-aligned dim key hint. The selected row gets
    /// the `surface` fill across the full menu width -- the single permitted
    /// tonal fill. No hairline dividers between rows.
    fn menu_row(&self, index: usize, menu_width: usize) -> Vec<Span<'static>> {
        let (action, label, hint) = MENU_ITEMS[index];
        // Keymap honesty: without the keyboard-enhancement protocol a plain
        // terminal cannot transmit ctrl+comma at all, so that hint would be a
        // dead control. Advertise the slash command instead — every printed
        // control must work.
        let hint = if action == StartAction::Settings && !self.punctuation_chords {
            "/settings"
        } else {
            hint
        };
        let selected = index == self.selected;
        let marker = if selected {
            Span::styled(format!("{} ", symbols::ACTIVE), prompt_style())
        } else {
            Span::raw("  ")
        };
        let label_style = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        // A dim recoverable-task badge (`· N to recover`) sits between the label
        // and the leader on the Tasks row, so the count is legible from the home
        // menu without a modal. It counts toward the leader budget below.
        let badge = (action == StartAction::Tasks && self.recoverable > 0)
            .then(|| format!(" · {} to recover", self.recoverable));
        let badge_width = badge.as_deref().map(str::len).unwrap_or(0);
        // marker(2) + label + badge + space + leader + space + hint.
        let leader_width = menu_width
            .saturating_sub(2)
            .saturating_sub(label.len())
            .saturating_sub(badge_width)
            .saturating_sub(hint.len())
            .saturating_sub(2);
        let mut spans = vec![marker, Span::styled(label.to_string(), label_style)];
        if let Some(badge) = badge {
            spans.push(Span::styled(badge, dim_style()));
        }
        spans.extend([
            Span::raw(" "),
            Span::styled("·".repeat(leader_width), dim_style()),
            Span::raw(" "),
            Span::styled(hint.to_string(), dim_style()),
        ]);
        if selected {
            let fill = Style::default().bg(palette::surface());
            for span in &mut spans {
                span.style = span.style.patch(fill);
            }
        }
        spans
    }
}

/// Center `spans` in `width` columns by left padding (never right padding, so a
/// selected row's surface fill ends with its content). Lines are truncated to
/// `width`: the terminal surface rejects over-width rows, so a very narrow
/// pane degrades to a clipped launcher instead of a failed render.
fn centered(spans: Vec<Span<'static>>, content_width: usize, width: usize) -> Line<'static> {
    let pad = width.saturating_sub(content_width) / 2;
    let mut line_spans = Vec::with_capacity(spans.len() + 1);
    if pad > 0 {
        line_spans.push(Span::raw(" ".repeat(pad)));
    }
    line_spans.extend(spans);
    let mut line = Line::from(line_spans);
    truncate_line(&mut line, width.max(1));
    line
}

impl Component for StartPage {
    /// The faceplate block: the IrisMark row, its silkscreen row, one blank
    /// row, then the menu rows, all centered in `width`.
    fn render(&self, width: usize) -> Vec<Line<'static>> {
        let mark_width = MARK_DOTS * 2 - 1;
        let menu_width = MENU_WIDTH.min(width.saturating_sub(2)).max(12);
        let mut lines = vec![
            centered(self.mark_spans(), mark_width, width),
            centered(self.silkscreen_spans(), mark_width, width),
            Line::default(),
        ];
        for index in 0..MENU_ITEMS.len() {
            lines.push(centered(
                self.menu_row(index, menu_width),
                menu_width,
                width,
            ));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::super::wrap::line_text;
    use super::*;

    #[test]
    fn ping_pong_reverses_at_both_ends_and_never_wraps() {
        let mut page = StartPage::new(false, 0, true);
        let mut seen = vec![page.head()];
        for _ in 0..(MARK_DOTS * 4) {
            page.advance_for_test();
            seen.push(page.head());
        }
        // Every step moves exactly one cell (never a wrap from 11 to 0).
        for pair in seen.windows(2) {
            assert_eq!(
                pair[0].abs_diff(pair[1]),
                1,
                "head must move one dot per advance: {seen:?}"
            );
            assert!(pair[1] < MARK_DOTS, "{seen:?}");
        }
        // The sweep reaches the right end and comes back to the left end.
        assert!(seen.contains(&(MARK_DOTS - 1)), "{seen:?}");
        assert!(
            seen.iter().filter(|&&h| h == 0).count() >= 2,
            "the head returns to the left end: {seen:?}"
        );
    }

    #[test]
    fn loop_tick_cadence_preserves_the_130ms_phase() {
        let mut page = StartPage::new(false, 0, true);
        let start = Instant::now();

        assert!(page.tick_at(start));
        assert_eq!(page.head(), 1);
        assert!(!page.tick_at(start + Duration::from_millis(100)));
        assert_eq!(page.head(), 1);
        assert!(page.tick_at(start + Duration::from_millis(200)));
        assert_eq!(page.head(), 2);
        assert!(page.tick_at(start + Duration::from_millis(300)));
        assert_eq!(page.head(), 3);
        assert!(page.tick_at(start + Duration::from_millis(400)));
        assert_eq!(page.head(), 4);
        assert!(!page.tick_at(start + Duration::from_millis(500)));
        assert_eq!(page.head(), 4);
        assert!(page.tick_at(start + Duration::from_millis(600)));
        assert_eq!(page.head(), 5);
    }

    #[test]
    fn reduced_motion_holds_a_static_center_dot() {
        let mut page = StartPage::new(true, 0, true);
        assert!(!page.tick(), "reduced motion never animates");
        let lines = page.render(80);
        let mark = line_text(&lines[0]);
        // Exactly one lit dot, at the strip center.
        assert_eq!(mark.matches('●').count(), 1, "{mark:?}");
        let cells: Vec<char> = mark.trim_start().chars().step_by(2).collect();
        assert_eq!(cells.len(), MARK_DOTS);
        assert_eq!(cells[MARK_DOTS / 2], '●', "{mark:?}");
    }

    #[test]
    fn launcher_never_renders_over_width_at_narrow_panes() {
        use super::super::wrap::display_width;
        let page = StartPage::new(false, 0, true);
        for width in 1..=(MENU_WIDTH + 4) {
            for line in page.render(width) {
                let text = line_text(&line);
                assert!(display_width(&text) <= width, "width {width}: {text:?}");
            }
        }
    }

    #[test]
    fn enabling_reduced_motion_freezes_the_sweep_at_center() {
        let mut page = StartPage::new(false, 0, true);
        page.advance_for_test();
        page.advance_for_test();
        assert_ne!(page.head(), MARK_DOTS / 2);

        page.set_reduced_motion(true);

        assert!(!page.tick());
        let mark = line_text(&page.render(80)[0]);
        assert_eq!(mark.matches('●').count(), 1, "{mark:?}");
        let cells: Vec<char> = mark.trim_start().chars().step_by(2).collect();
        assert_eq!(cells[MARK_DOTS / 2], '●', "{mark:?}");
        assert!(line_text(&page.render(80)[3]).contains("New session"));
    }

    #[test]
    fn silkscreen_prints_the_wordmark_and_rev_on_the_strip_measure() {
        use super::super::wrap::display_width;
        let page = StartPage::new(true, 0, true);
        let lines = page.render(80);
        let silkscreen = line_text(&lines[1]);
        assert!(silkscreen.contains(WORDMARK), "{silkscreen:?}");
        assert!(silkscreen.contains(REV), "{silkscreen:?}");
        // The row sits on the strip measure: same width, same left edge.
        let mark = line_text(&lines[0]);
        let mark_indent = mark.len() - mark.trim_start().len();
        let silk_indent = silkscreen.len() - silkscreen.trim_start().len();
        assert_eq!(mark_indent, silk_indent, "left edges align");
        assert_eq!(
            display_width(silkscreen.trim_end()) - silk_indent,
            MARK_DOTS * 2 - 1,
            "right edges align: {silkscreen:?}"
        );
    }

    #[test]
    fn launcher_selection_wraps_both_ways() {
        let mut page = StartPage::new(true, 0, true);
        assert_eq!(page.selected_action(), StartAction::NewSession);
        page.up();
        assert_eq!(page.selected_action(), StartAction::Quit);
        page.down();
        assert_eq!(page.selected_action(), StartAction::NewSession);
        page.down();
        assert_eq!(page.selected_action(), StartAction::ResumeSession);
        page.down();
        assert_eq!(page.selected_action(), StartAction::Tasks);
        page.down();
        assert_eq!(page.selected_action(), StartAction::Settings);
        assert_eq!(page.selected(), 3);
    }

    #[test]
    fn launcher_rows_carry_marker_leader_and_key_hints() {
        let page = StartPage::new(true, 0, true);
        let lines = page.render(80);
        // Mark, silkscreen, blank, then the five menu rows.
        assert_eq!(lines.len(), 3 + 5);
        let first = line_text(&lines[3]);
        assert!(first.contains("◉ New session"), "{first:?}");
        assert!(first.contains("···"), "dotted leader: {first:?}");
        assert!(first.trim_end().ends_with("ctrl-n"), "{first:?}");
        let second = line_text(&lines[4]);
        assert!(!second.contains('◉'), "only the selected row is marked");
        assert!(second.contains("Resume session"), "{second:?}");
        assert!(second.trim_end().ends_with("ctrl-r"), "{second:?}");
        let tasks = line_text(&lines[5]);
        assert!(tasks.contains("Tasks"), "{tasks:?}");
        assert!(tasks.trim_end().ends_with("ctrl-t"), "{tasks:?}");
        assert!(line_text(&lines[6]).trim_end().ends_with("ctrl-,"));
        assert!(line_text(&lines[7]).trim_end().ends_with("ctrl-q"));
    }

    #[test]
    fn tasks_row_shows_a_recoverable_badge_only_when_nonzero() {
        // No recoverable tasks: the Tasks row is a plain launcher row.
        let none = StartPage::new(true, 0, true);
        let tasks = line_text(&none.render(80)[5]);
        assert!(!tasks.contains("to recover"), "{tasks:?}");

        // With recoverable tasks: a dim `· N to recover` badge, still ending in
        // the key hint, and the row never renders over width.
        let some = StartPage::new(true, 2, true);
        let tasks = line_text(&some.render(80)[5]);
        assert!(tasks.contains("· 2 to recover"), "{tasks:?}");
        assert!(tasks.trim_end().ends_with("ctrl-t"), "{tasks:?}");
        for width in 1..=(MENU_WIDTH + 4) {
            for line in some.render(width) {
                let text = line_text(&line);
                assert!(
                    super::super::wrap::display_width(&text) <= width,
                    "width {width}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn selected_row_uses_the_surface_fill_and_bold_label() {
        let page = StartPage::new(true, 0, true);
        let lines = page.render(80);
        let selected = &lines[3];
        assert!(
            selected
                .spans
                .iter()
                .filter(|span| !span.content.trim().is_empty() || span.style.bg.is_some())
                .all(|span| span.style.bg == Some(palette::surface())),
            "selected row carries the surface fill: {selected:?}"
        );
        assert!(
            selected.spans.iter().any(|span| {
                span.content.as_ref() == "New session"
                    && span.style.add_modifier.contains(Modifier::BOLD)
            }),
            "{selected:?}"
        );
        let unselected = &lines[4];
        assert!(
            unselected.spans.iter().all(|span| span.style.bg.is_none()),
            "{unselected:?}"
        );
    }

    #[test]
    fn trail_follows_behind_the_travel_direction() {
        let mut page = StartPage::new(false, 0, true);
        page.advance_for_test();
        page.advance_for_test();
        page.advance_for_test();
        assert_eq!(page.head(), 3);
        let mark = &page.render(40)[0];
        // head bold-orange at cell 3; trail-1 at 2 (orange), trail-2 at 1.
        let cell_style = |cell: usize| {
            let mut lit = mark
                .spans
                .iter()
                .filter(|span| !span.content.trim().is_empty());
            lit.nth(cell).map(|span| span.style).expect("cell")
        };
        assert!(cell_style(3).add_modifier.contains(Modifier::BOLD));
        assert_eq!(cell_style(3).fg, Some(palette::orange()));
        assert_eq!(cell_style(2).fg, Some(palette::orange()));
        assert!(!cell_style(2).add_modifier.contains(Modifier::BOLD));
        // trail-2 is a dim orange (the comet fades in its own hue), not grey.
        assert_eq!(cell_style(1).fg, Some(palette::orange()));
        assert!(cell_style(1).add_modifier.contains(Modifier::DIM));
    }
}
