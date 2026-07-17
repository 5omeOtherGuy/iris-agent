//! Alt-screen pager surface (ADR-0029).
//!
//! Two layers, split so each is testable without a TTY:
//!
//! - [`AltScreen`]: the alternate-screen lifecycle -- enter (`?1049h` + clear +
//!   home), leave (`?1049l`), and panic-safe restore. Restore runs through
//!   three independent paths that must all be idempotent: normal
//!   shutdown/`Drop`, the process panic hook, and the force-quit signal
//!   handler (`crate::signals`, which owns the async-signal-safe byte write).
//!   A single global "alt screen active" flag in `signals` arbitrates so
//!   exactly one path emits the leave sequence. Byte-golden testable over any
//!   `Write`.
//! - [`PagerSurface`]: the production full-frame renderer -- a ratatui
//!   `Terminal<CrosstermBackend<Stdout>>` drawing [`compose_frame`] output
//!   inside `?2026` synchronized-update blocks, with stock cell diffing.
//!   The frame composition ([`compose_frame`]) and cell placement
//!   ([`render_frame`]) are pure and golden-frame tested on a `TestBackend`.
//!
//! The pager renders the SAME logical document as the inline surface
//! ([`super::screen`]'s `render_document_with_hints`), sliced to the viewport.
//! Normal posture pins the session bar at the top and the working indicator plus
//! composer at the bottom around a scrollable transcript. Focus posture removes
//! the top bar and collapses an empty composer to one bottom metadata row.

use std::io::{self, Stdout, Write};
use std::sync::Once;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{MoveTo, Show};
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::terminal::{
    BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen, disable_raw_mode,
};
use ratatui::crossterm::{execute, queue};
use ratatui::layout::Size;
use ratatui::text::Line;

use ratatui::style::Modifier;
use ratatui::text::Span;

use super::screen::{
    Screen, ambient_worker_lane_block, filler_lines, render_editor_chrome, session_bar_lines,
};
use super::wrap::{display_width, ellipsize_to_width, pad_line_left, truncate_to_width};
use super::{BOX_X_PADDING_U16, TEXT_COLUMN_X_PADDING, dim_style, panel_style};

/// Iris-owned scrollback state for pager mode (ADR-0029).
///
/// `top_offset` is the visible-line index of the viewport top within the
/// transcript (offset-from-top): appends below the viewport never move an
/// anchored view, which is also what keeps a fold of the latest (bottom)
/// panel from shifting an anchored reader (grok's `anchor_on_fold`).
/// `follow` pins the view to the live tail; any upward scroll disengages it
/// and scrolling back past the bottom re-engages it (grok's
/// `follow_by_overscroll`).
#[derive(Debug)]
pub(crate) struct ScrollState {
    top_offset: usize,
    follow: bool,
    /// Layout metrics from the last composed frame, so key handling clamps
    /// against the real wrapped layout without recomputing it.
    view_rows: usize,
    total: usize,
    /// Virtual rows reserved at the bottom of the viewport for an overlay that
    /// covers the last body row (the centered `/find` indicator). While set,
    /// the scrollable range extends by this many rows so the tail -- including
    /// a match on the very last transcript line -- can sit ABOVE the overlay
    /// instead of being drawn into the row it overwrites. `0` (the default,
    /// and whenever no search is active) leaves all scrolling/follow behavior
    /// identical to a plain viewport.
    bottom_pad: usize,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            top_offset: 0,
            follow: true,
            view_rows: 0,
            total: 0,
            bottom_pad: 0,
        }
    }
}

impl ScrollState {
    pub(crate) fn is_following(&self) -> bool {
        self.follow
    }

    /// Record the frame layout and clamp the offset. Called once per compose;
    /// a shrunken transcript (session swap) or grown viewport re-engages
    /// follow when nothing is left to scroll.
    pub(in crate::ui) fn sync(&mut self, total: usize, view_rows: usize) {
        self.total = total;
        self.view_rows = view_rows;
        let max_top = self.max_top();
        self.top_offset = self.top_offset.min(max_top);
        if max_top == 0 {
            self.follow = true;
        }
    }

    /// Reserve `pad` bottom rows for an overlay that covers the last body row
    /// (the `/find` indicator), extending the scrollable range so the tail can
    /// clear it. Set once per compose from the active search state; `0`
    /// restores plain-viewport behavior.
    pub(in crate::ui) fn set_bottom_pad(&mut self, pad: usize) {
        self.bottom_pad = pad;
    }

    /// Greatest viewport-top offset. Reserves `bottom_pad` rows below the
    /// transcript so a bottom overlay does not swallow the last line; with no
    /// reservation this is the plain `total - view_rows`.
    fn max_top(&self) -> usize {
        self.total
            .saturating_sub(self.view_rows.saturating_sub(self.bottom_pad))
    }

    /// The viewport-top line index for the current frame.
    pub(super) fn top(&self) -> usize {
        if self.follow {
            self.max_top()
        } else {
            self.top_offset.min(self.max_top())
        }
    }

    /// Visible lines below the viewport (0 while following).
    fn lines_below(&self) -> usize {
        self.total
            .saturating_sub(self.top() + self.view_rows.min(self.total))
    }

    /// Scroll up `n` lines; disengages follow.
    pub(crate) fn scroll_up(&mut self, n: usize) {
        self.top_offset = self.top().saturating_sub(n);
        self.follow = false;
    }

    /// Scroll down `n` lines; reaching (or overshooting) the bottom re-engages
    /// follow (`follow_by_overscroll`).
    pub(crate) fn scroll_down(&mut self, n: usize) {
        if self.follow {
            return;
        }
        self.top_offset = self.top_offset.saturating_add(n);
        let max_top = self.max_top();
        if self.top_offset >= max_top {
            self.top_offset = max_top;
            self.follow = true;
        }
    }

    pub(crate) fn page_up(&mut self) {
        self.scroll_up(self.view_rows.max(1));
    }

    pub(crate) fn page_down(&mut self) {
        self.scroll_down(self.view_rows.max(1));
    }

    /// Jump to the transcript start (disengages follow while there is history
    /// below).
    pub(crate) fn jump_to_start(&mut self) {
        self.top_offset = 0;
        self.follow = self.max_top() == 0;
    }

    /// Jump to the live tail and re-engage follow.
    pub(crate) fn follow_latest(&mut self) {
        self.follow = true;
    }

    /// Scroll the minimum distance so visible line `line` is inside the
    /// viewport (selection keep-visible). A reveal that lands at the bottom
    /// re-engages follow; one that scrolls up disengages it.
    pub(in crate::ui) fn reveal(&mut self, line: usize) {
        self.reveal_with_bottom_margin(line, 0);
    }

    /// Like [`Self::reveal`], but keeps `bottom_margin` rows clear below the
    /// target. A `/find` jump reserves the one row the centered search
    /// indicator overwrites (`body[view_rows - 1]`), so a match revealed near
    /// the tail lands ABOVE the indicator and keeps its highlight instead of
    /// being covered by it.
    pub(in crate::ui) fn reveal_with_bottom_margin(&mut self, line: usize, bottom_margin: usize) {
        let top = self.top();
        let view = self.view_rows.saturating_sub(bottom_margin);
        if line < top {
            self.top_offset = line;
            self.follow = false;
        } else if view > 0 && line >= top + view {
            let max_top = self.max_top();
            self.top_offset = (line + 1 - view).min(max_top);
            self.follow = self.top_offset >= max_top;
        }
    }
}

/// Owns the alternate-screen lifecycle for pager mode. The writer is a second
/// handle to the same terminal the `TerminalSurface` writes through; this type
/// only enters/leaves the alt screen and never renders content itself.
pub(crate) struct AltScreen<W: Write> {
    writer: W,
    active: bool,
}

impl<W: Write> AltScreen<W> {
    /// Enter the alternate screen: `?1049h`, clear, cursor home. The global
    /// active flag is set so the panic hook and the force-quit signal path
    /// know a leave is owed.
    pub(crate) fn enter(mut writer: W) -> io::Result<Self> {
        // Mark active BEFORE writing: a partial write/flush failure may still
        // have delivered `?1049h`, so a leave is owed from the first byte. On
        // failure, best-effort leave immediately and clear the pending flag.
        crate::signals::set_alt_screen_active(true);
        let entered = queue!(
            writer,
            EnterAlternateScreen,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )
        .and_then(|()| writer.flush());
        if let Err(error) = entered {
            if crate::signals::take_alt_screen_active() {
                let _ = queue!(writer, LeaveAlternateScreen);
                let _ = writer.flush();
            }
            return Err(error);
        }
        Ok(Self {
            writer,
            active: true,
        })
    }

    /// Leave the alternate screen exactly once across all restore paths: the
    /// local flag makes repeated `leave`/`Drop` calls no-ops, and the global
    /// take keeps this path from double-emitting after the panic hook already
    /// restored the screen.
    pub(crate) fn leave(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        if !crate::signals::take_alt_screen_active() {
            return Ok(());
        }
        queue!(self.writer, LeaveAlternateScreen)?;
        self.writer.flush()
    }
}

impl<W: Write> Drop for AltScreen<W> {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

/// Install the process panic hook that restores the terminal before the
/// default hook prints the panic message -- otherwise the message would be
/// written to the alternate screen and vanish with it, and the user's shell
/// would be left inside a dead alt screen. Installed once, chains the previous
/// hook, and is a strict no-op while the pager is not active.
pub(crate) fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = emergency_restore(&mut io::stdout());
            previous(info);
        }));
    });
}

/// Leave the alt screen and show the cursor if (and only if) the pager is
/// active; also drops raw mode so the panic output is readable. Consumes the
/// global flag, making every later restore path a no-op.
fn emergency_restore<W: Write>(writer: &mut W) -> io::Result<()> {
    if !crate::signals::take_alt_screen_active() {
        return Ok(());
    }
    let _ = disable_raw_mode();
    // Mouse capture is on by default in pager mode; drop it before leaving so
    // a panic never strands the terminal reporting mouse escapes at the shell.
    queue!(writer, DisableMouseCapture, LeaveAlternateScreen, Show)?;
    writer.flush()
}

/// Emit the mouse-capture enable/disable sequence (SGR + motion modes via
/// crossterm). Pager mode only; the inline surface never captures the mouse.
pub(super) fn set_mouse_capture<W: Write>(writer: &mut W, on: bool) -> io::Result<()> {
    if on {
        queue!(writer, EnableMouseCapture)?;
    } else {
        queue!(writer, DisableMouseCapture)?;
    }
    writer.flush()
}

/// Production pager renderer: alt-screen lifecycle + a ratatui `Terminal`
/// drawing full frames with stock cell diffing. Stdout-only by design; the
/// pure pieces ([`compose_frame`], [`render_frame`]) carry the tests.
pub(crate) struct PagerSurface {
    /// Alt-screen guard. Held (and dropped) alongside the terminal so leaving
    /// the alt screen is ordered after the last frame.
    alt: AltScreen<Stdout>,
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl PagerSurface {
    /// Enter the alternate screen and build the fullscreen ratatui terminal
    /// over stdout. On terminal construction failure the guard's `Drop`
    /// restores the normal screen.
    pub(crate) fn enter() -> io::Result<Self> {
        let alt = AltScreen::enter(io::stdout())?;
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        Ok(Self { alt, terminal })
    }

    /// Draw one full frame inside a `?2026` synchronized-update block. The
    /// frame is composed INSIDE the draw closure from the autoresized
    /// `frame.area()`, so slicing and rendering always agree on the viewport
    /// (a resize between a size query and the draw cannot unpin the composer).
    pub(crate) fn render_with(
        &mut self,
        compose: impl FnOnce(Size) -> ComposedFrame,
    ) -> io::Result<()> {
        execute!(self.terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let mut cursor = None;
        let drawn = self
            .terminal
            .draw(|frame| {
                let area = frame.area();
                let composed = compose(Size::new(area.width, area.height));
                render_frame(frame, &composed.lines);
                cursor = composed.cursor;
            })
            .map(|_| ());
        // Position (never show) the hardware cursor at the composer's marker
        // for IME candidate-window placement, mirroring the inline surface's
        // `position_hardware_cursor` (the cursor stays hidden; Iris draws its
        // own reversed block cursor).
        let positioned = match cursor {
            Some((column, row)) => queue!(self.terminal.backend_mut(), MoveTo(column, row))
                .and_then(|()| self.terminal.backend_mut().flush()),
            None => Ok(()),
        };
        // Always close the sync block, even when the draw failed, so an error
        // can never leave the terminal buffering forever.
        let ended = execute!(self.terminal.backend_mut(), EndSynchronizedUpdate);
        drawn.and(positioned).and(ended)
    }

    /// Leave the alternate screen (idempotent; also covered by `Drop`).
    pub(crate) fn leave(&mut self) -> io::Result<()> {
        self.alt.leave()
    }
}

/// One composed pager frame: the viewport lines plus the hardware-cursor
/// position (column, row) extracted from the composer's zero-width marker,
/// when the composer is focused.
pub(super) struct ComposedFrame {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) cursor: Option<(u16, u16)>,
}

/// Compose the pager frame from the same components as the inline document.
/// Normal posture pins the session bar above the transcript window and the
/// working indicator plus composer below it. Focus posture gives the top rows to
/// the transcript and replaces an empty composer with its compact metadata row.
/// The transcript is rendered visible-range-only through the wrap cache, so
/// frame cost is O(viewport), independent of transcript length (ADR-0029).
pub(super) fn compose_frame(screen: &mut Screen, size: Size) -> ComposedFrame {
    let width = size.width.max(1);
    let height = usize::from(size.height.max(1));
    // Thread the frame height into the transcript so a preview built in the
    // `apply` path breathes with the pager viewport too (reactive-density §2).
    screen.transcript.note_pane_height(size.height.max(1));

    let bar = session_bar_lines(screen, width, size.height.max(1));
    let bar_rows = bar.len().min(height);
    // The ambient worker card is pinned separately from the bar: it renders
    // BELOW the sticky prompt band (see the frame assembly), so the governing
    // prompt keeps the topmost position and never shifts while the card
    // appears, updates, or retires.
    let lane = ambient_worker_lane_block(screen, width);
    let lane_rows = lane.len().min(height.saturating_sub(bar_rows));

    // Bottom-pinned tail: blank-padded working indicator + composer chrome
    // (which carries the docked overlays/modals), exactly as inline.
    let working = screen.working_lines(width);
    let mut tail: Vec<Line<'static>> = Vec::new();
    if !working.is_empty() {
        tail.push(Line::default());
        tail.extend(working);
        tail.push(Line::default());
    }
    tail.extend(render_editor_chrome(screen, width, size.height.max(1)));
    // On very short viewports keep the BOTTOM of the tail (statusline edge),
    // mirroring the inline surface's bottom-anchored behavior.
    let tail_budget = height.saturating_sub(bar_rows + lane_rows);
    if tail.len() > tail_budget {
        tail.drain(..tail.len() - tail_budget);
    }

    let view_rows = tail_budget - tail.len();
    let total = screen.transcript_visible_total(width);
    // An active search draws its indicator over the last body row, so reserve
    // that row: the scrollable range extends by one and a tail-adjacent (or
    // final-line) match can land above the indicator instead of under it.
    let search_pad = usize::from(screen.search.is_some());
    screen.scroll.set_bottom_pad(search_pad);
    screen.scroll.sync(total, view_rows);
    // Keep the selected scrollback entry visible (the wrap cache is warm
    // after `transcript_visible_total`, so the line lookup is O(1)).
    let selected_line = if screen.scrollback_focus {
        screen
            .normalized_selection()
            .and_then(|row| screen.transcript_line_of_row(row))
    } else {
        None
    };
    if let Some(line) = selected_line {
        screen.scroll.reveal(line);
    }
    // One-shot reveal queued by a search jump (`/find`, n/N): scroll the
    // match into view without pinning the view there afterwards. Reserve the
    // bottom row the search indicator occupies so a tail-adjacent match is not
    // hidden behind it.
    if let Some(line) = screen.reveal_line.take() {
        screen
            .scroll
            .reveal_with_bottom_margin(line.min(total.saturating_sub(1)), 1);
    }
    let top = screen.scroll.top();

    let mut body = screen.transcript_window(width, top, view_rows);
    // Pointer ownership follows the composed frame, not the transcript slice.
    // Start with only rows actually supplied by the transcript; filler, pinned
    // tail chrome, and every later overlay remain non-interactive unless they
    // explicitly register their own target.
    let transcript_body_rows = body.len();
    let mut transcript_hit_rows = vec![false; view_rows];
    transcript_hit_rows[..transcript_body_rows.min(view_rows)].fill(true);
    screen.pager_sticky_hit_row = None;
    if body.len() < view_rows {
        // Blank filler (or the centered start page) sits BETWEEN the
        // transcript and the tail, exactly as in the inline document.
        body.extend(filler_lines(screen, view_rows - body.len(), width));
    }
    // Focused-scrollback selection highlight: the selected entry's header
    // line gets the surface fill (the single permitted tonal selection fill).
    if let Some(line) = selected_line
        && line >= top
        && line - top < body.len()
    {
        for span in &mut body[line - top].spans {
            span.style = span.style.bg(crate::ui::palette::surface());
        }
    }
    // Current search match: surface fill on the whole match line.
    if let Some(line) = screen.search.as_ref().and_then(|state| state.line)
        && line >= top
        && line - top < body.len()
    {
        for span in &mut body[line - top].spans {
            span.style = span.style.bg(crate::ui::palette::surface());
        }
    }
    // Sticky user-prompt band (grok `sticky_headers`): when the newest prompt
    // above the viewport has scrolled past the top, pin it as a dim band under
    // the session bar so the reader always knows which prompt the visible content
    // answers. It reads as an extension of the top chrome, not a card floating in
    // the transcript. Interactive transcript state wins the entire band
    // footprint: a selection or search hit under any expanded continuation or
    // rule row keeps its highlight, and the card yields as one stable overlay.
    let mut band_rows = 0usize;
    if view_rows >= 5
        && top > 0
        && let Some(text) = screen.transcript.sticky_prompt_text(top)
    {
        let band = sticky_prompt_band(text, width, view_rows, screen.sticky_prompt_expanded);
        let band_end = top.saturating_add(band.len());
        let search_line = screen.search.as_ref().and_then(|state| state.line);
        let overlaps =
            |line: Option<usize>| line.is_some_and(|line| line >= top && line < band_end);
        if !overlaps(selected_line) && !overlaps(search_line) {
            let painted = band.len().min(body.len());
            for (dst, line) in body.iter_mut().zip(band) {
                *dst = line;
            }
            transcript_hit_rows[..painted].fill(false);
            if painted > 0 {
                screen.pager_sticky_hit_row = u16::try_from(bar_rows).ok();
            }
            band_rows = painted;
        }
    }
    // Bottom overlay row: an active search shows its position indicator;
    // otherwise disengaged follow shows how much is below. Search wins (it is
    // the mode the user just entered).
    if view_rows > 0 {
        if let Some(state) = screen.search.as_ref() {
            body[view_rows - 1] = search_indicator_line(state, width);
            transcript_hit_rows[view_rows - 1] = false;
        } else if !screen.scroll.is_following() {
            let below = screen.scroll.lines_below();
            if below > 0 {
                body[view_rows - 1] = follow_indicator_line(below, width);
                transcript_hit_rows[view_rows - 1] = false;
            }
        }
    }

    // Resolve visible physical header rows now, after every overlay has claimed
    // its cells. This same frame map is consumed by mouse input; it therefore
    // survives arbitrary tail height, menu growth, and live terminal resizing
    // without a second layout calculation drifting from what the user sees.
    screen.pager_header_hits = transcript_hit_rows
        .iter()
        .enumerate()
        .filter_map(|(offset, owns_transcript)| {
            if !owns_transcript {
                return None;
            }
            let header = screen
                .transcript
                .header_row_at_visible_line(top.saturating_add(offset))?;
            // Rows below the band are pushed down by the worker card.
            let shift = if offset >= band_rows { lane_rows } else { 0 };
            let row = u16::try_from(bar_rows.saturating_add(offset).saturating_add(shift)).ok()?;
            Some((row, header))
        })
        .collect();

    let mut frame = Vec::with_capacity(height);
    frame.extend(bar.into_iter().take(bar_rows));
    // The worker card mounts between the pinned prompt band and the scrolled
    // transcript content; with no band it sits directly under the bar block.
    let rest = body.split_off(band_rows.min(body.len()));
    frame.extend(body);
    frame.extend(lane.into_iter().take(lane_rows));
    frame.extend(rest);
    frame.extend(tail);

    // OSC 8 hyperlink markers are stripped from the frame here and their
    // visible column regions recorded for mouse hit-testing: the ratatui
    // `Buffer` the frame is drawn into cannot carry OSC 8, so the pager resolves
    // clicks against these regions and opens via the `open_in_browser`/notice
    // seam instead. Done before the cursor-marker strip; the two marker kinds
    // are independent zero-width spans.
    screen.pager_links = crate::ui::hyperlink::extract_and_strip_lines(&mut frame);

    // The zero-width hardware-cursor marker is located and stripped here (the
    // inline surface does this in its line renderer); its position drives IME
    // candidate-window placement. Bounded scan: at most one viewport of lines.
    let marker = crate::ui::terminal_surface::CURSOR_MARKER;
    let cursor = super::component::take_marker_position(&mut frame, marker)
        .map(|(row, column)| (column.min(usize::from(u16::MAX)) as u16, row as u16));
    // Defensive: strip any further markers so none can reach the cells.
    while super::component::take_marker_position(&mut frame, marker).is_some() {}
    ComposedFrame {
        lines: frame,
        cursor,
    }
}

/// The pinned prompt band -- the governing user prompt (the "job card") under
/// the session bar. Collapsed, it is a compact prompt row plus a closing hairline:
/// `▸ ` / `› ` chrome in muted bold, the prompt's opening line in body ink
/// (`panel_style`), and a right-aligned dim `+N` counting the wrapped rows it
/// hides. A click on the row or `o` (in pager mode) expands it to the full prompt
/// body on the transcript's hanging columns. Both postures close with the session
/// bar's own muted rule, so the governing prompt always reads as one bounded
/// instrument surface. ctrl+o never routes here -- it toggles transcript folds.
fn sticky_prompt_band(
    text: &str,
    width: u16,
    max_rows: usize,
    expanded: bool,
) -> Vec<Line<'static>> {
    let width = usize::from(width);
    let text_measure = width
        .saturating_sub(TEXT_COLUMN_X_PADDING.saturating_mul(2))
        .saturating_sub(4)
        .max(1);
    let inset = usize::from(BOX_X_PADDING_U16).min(width.saturating_sub(1));
    let rule_measure = width.saturating_sub(inset.saturating_mul(2)).max(1);

    let mut wrapped: Vec<String> = text
        .split('\n')
        .flat_map(|logical| crate::ui::textengine::wrap_to_width(logical, text_measure))
        .collect();
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }

    let dim = dim_style();
    let ink = panel_style();
    let chrome = dim.add_modifier(Modifier::BOLD);
    let arrow = if expanded {
        crate::ui::symbols::EXPANDED
    } else {
        crate::ui::symbols::COLLAPSED
    };
    let mut rows = Vec::new();

    // First row: `▸ ` / `› ` chrome (muted bold) then the prompt's opening line in
    // body ink (`panel_style`) -- the one legible piece of content in the top
    // chrome, so the job card reads at a glance while its tones still read as
    // chrome. Collapsed, a right-aligned dim `+N` reports how many wrapped rows
    // stay hidden (the house `+N more` idiom, shortened); it is absent when the
    // prompt fits one row.
    let hidden = if expanded {
        0
    } else {
        wrapped.len().saturating_sub(1)
    };
    let mut first_spans = vec![
        Span::styled(format!("{arrow} "), chrome),
        Span::styled(format!("{} ", crate::ui::symbols::USER), chrome),
    ];
    if hidden > 0 {
        let marker = format!("+{hidden}");
        let marker_width = display_width(&marker);
        // Reserve the marker plus a one-cell gap on the right, then truncate the
        // opening line with `…` so the ink text never collides with the dim `+N`.
        let text_budget = text_measure.saturating_sub(marker_width + 1).max(1);
        let mut opening = wrapped[0].clone();
        if display_width(&opening) > text_budget {
            opening = format!(
                "{}\u{2026}",
                truncate_to_width(&opening, text_budget.saturating_sub(1))
            );
        }
        let filler = text_measure
            .saturating_sub(display_width(&opening))
            .saturating_sub(marker_width);
        first_spans.push(Span::styled(opening, ink));
        first_spans.push(Span::raw(" ".repeat(filler)));
        first_spans.push(Span::styled(marker, dim));
    } else {
        first_spans.push(Span::styled(wrapped[0].clone(), ink));
    }
    let mut first = Line::from(first_spans);
    pad_line_left(&mut first, TEXT_COLUMN_X_PADDING);
    rows.push(first);

    if expanded {
        for part in wrapped.into_iter().skip(1) {
            let mut line = Line::from(vec![Span::raw("    "), Span::styled(part, ink)]);
            pad_line_left(&mut line, TEXT_COLUMN_X_PADDING);
            rows.push(line);
        }
    }
    // The closing rule stays muted -- it is top chrome, not content. Keeping
    // it in the collapsed posture prevents the one-line card from dissolving
    // into transcript rows immediately below it.
    let mut rule = Line::from(Span::styled("─".repeat(rule_measure), dim));
    pad_line_left(&mut rule, inset);
    rows.push(rule.clone());

    if rows.len() <= max_rows {
        return rows;
    }

    // Expanded prompts can be taller than the entire transcript viewport. The
    // old `truncate` silently removed both content and the closing boundary.
    // Preserve the rule and spend the penultimate row on an honest hidden-row
    // readout. The pager only mounts sticky bands with >=5 body rows; the 0–2
    // branches are defensive and still prefer disclosure over silent loss.
    let prompt_rows = rows.len().saturating_sub(1);
    match max_rows {
        0 => Vec::new(),
        1 => vec![sticky_hidden_rows_line(prompt_rows, width)],
        2 => vec![sticky_hidden_rows_line(prompt_rows, width), rule],
        _ => {
            let shown_prompt_rows = max_rows - 2;
            let hidden = prompt_rows.saturating_sub(shown_prompt_rows);
            rows.truncate(shown_prompt_rows);
            rows.push(sticky_hidden_rows_line(hidden, width));
            rows.push(rule);
            rows
        }
    }
}

fn sticky_hidden_rows_line(hidden: usize, width: usize) -> Line<'static> {
    let noun = if hidden == 1 { "row" } else { "rows" };
    let budget = width
        .saturating_sub(TEXT_COLUMN_X_PADDING)
        .saturating_sub(4)
        .max(1);
    let label = ellipsize_to_width(&format!("… +{hidden} {noun}"), budget);
    let mut line = Line::from(vec![Span::raw("    "), Span::styled(label, dim_style())]);
    pad_line_left(&mut line, TEXT_COLUMN_X_PADDING);
    line
}

/// Muted centered search indicator. Query text yields first, with a grapheme-
/// safe ellipsis, while match position/state and closing quote stay legible.
fn search_indicator_line(state: &super::screen::SearchState, width: u16) -> Line<'static> {
    let width = usize::from(width);
    let query = state.query.escape_debug().to_string();
    let suffixes = if state.total == 0 {
        vec![" ─ no matches".to_string(), " no matches".to_string()]
    } else {
        vec![
            format!(
                " ─ {}/{} {} n older {} N newer",
                state.position,
                state.total,
                crate::ui::symbols::SEP,
                crate::ui::symbols::SEP
            ),
            format!(
                " ─ {}/{} {} n/N",
                state.position,
                state.total,
                crate::ui::symbols::SEP
            ),
            format!(" ─ {}/{}", state.position, state.total),
            format!(" {}/{}", state.position, state.total),
        ]
    };
    let prefix = "find \"";
    let label = suffixes
        .into_iter()
        .find_map(|suffix| {
            let fixed = display_width(prefix) + 1 + display_width(&suffix);
            let minimum_query = usize::from(!query.is_empty());
            (fixed + minimum_query <= width).then(|| {
                let query = ellipsize_to_width(&query, width - fixed);
                format!("{prefix}{query}\"{suffix}")
            })
        })
        .unwrap_or_else(|| {
            let fallback = if state.total == 0 {
                "no matches".to_string()
            } else {
                format!("{}/{}", state.position, state.total)
            };
            ellipsize_to_width(&fallback, width)
        });
    let pad = width.saturating_sub(display_width(&label)) / 2;
    Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(label, dim_style()),
    ])
}

/// Muted centered `▾ N lines below` indicator shown while follow is
/// disengaged, on the row just above the pinned tail.
fn follow_indicator_line(below: usize, width: u16) -> Line<'static> {
    let label = if below == 1 {
        format!("{} 1 line below", crate::ui::symbols::EXPANDED)
    } else {
        format!("{} {below} lines below", crate::ui::symbols::EXPANDED)
    };
    let width = usize::from(width);
    let label = ellipsize_to_width(&label, width);
    let pad = width.saturating_sub(display_width(&label)) / 2;
    Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(label, dim_style()),
    ])
}

/// Place composed lines into the frame buffer, top-aligned, truncated to the
/// frame area. Cells beyond the composed lines stay blank (ratatui resets the
/// back buffer each frame).
pub(super) fn render_frame(frame: &mut ratatui::Frame, lines: &[Line<'static>]) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    for (row, line) in lines.iter().take(usize::from(area.height)).enumerate() {
        buf.set_line(area.x, area.y + row as u16, line, area.width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::ToolCall;
    use crate::ui::UiEvent;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    /// The alt-screen active flag is process-global; the shared guard in
    /// `signals` serializes every test (in any module) that toggles it and
    /// resets the flag to inactive on acquisition.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::signals::alt_screen_test_guard()
    }

    /// Expected emergency-restore byte sequence, built from the same
    /// crossterm commands so the golden tracks crossterm's encoding.
    fn emergency_restore_bytes() -> Vec<u8> {
        let mut expected = Vec::new();
        queue!(expected, DisableMouseCapture, LeaveAlternateScreen, Show).expect("queue");
        expected
    }

    #[test]
    fn mouse_capture_toggle_emits_enable_and_disable_sequences() {
        let mut on = Vec::new();
        set_mouse_capture(&mut on, true).expect("enable");
        let mut expected_on = Vec::new();
        queue!(expected_on, EnableMouseCapture).expect("queue");
        assert_eq!(on, expected_on);
        assert!(
            String::from_utf8_lossy(&on).contains("\x1b[?1006h"),
            "SGR encoding mode is part of the enable sequence"
        );

        let mut off = Vec::new();
        set_mouse_capture(&mut off, false).expect("disable");
        let mut expected_off = Vec::new();
        queue!(expected_off, DisableMouseCapture).expect("queue");
        assert_eq!(off, expected_off);
    }

    fn footer_screen() -> Screen {
        let mut screen = Screen::new();
        screen.set_footer_with_context(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            Some(300_000),
            "~/repo (main)".to_string(),
        );
        screen
    }

    fn shell_history(screen: &mut Screen, count: usize) {
        for index in 0..count {
            screen.apply(UiEvent::ToolResult {
                call: ToolCall {
                    id: format!("shell-{index}"),
                    thought_signature: None,
                    name: "bash".to_string(),
                    arguments: json!({ "command": format!("printf row-{index}") }),
                },
                content: format!("row-{index}"),
                exit_code: Some(0),
                duration: None,
            });
        }
    }

    /// Put a real foldable header under `body_offset` without changing the
    /// frame's pinned chrome. The caller can then paint an overlay on that row
    /// and prove the click target follows the composed frame, not the hidden
    /// transcript geometry.
    fn align_header_under(
        screen: &mut Screen,
        size: Size,
        body_offset: usize,
        needs_rows_below: bool,
    ) -> usize {
        let _ = compose_frame(screen, size);
        let max_top = screen.scroll.max_top();
        let (header, line) = screen
            .transcript
            .panel_header_rows()
            .into_iter()
            .filter_map(|header| {
                screen
                    .transcript_line_of_row(header)
                    .map(|line| (header, line))
            })
            .find(|(_, line)| {
                *line >= body_offset
                    && line.saturating_sub(body_offset) <= max_top
                    && (!needs_rows_below || line.saturating_sub(body_offset) < max_top)
            })
            .expect("a header can be aligned under the overlay");
        screen.scroll.top_offset = line - body_offset;
        screen.scroll.follow = false;
        header
    }

    #[test]
    fn long_unicode_search_readout_keeps_state_and_uses_themed_muted_ink() {
        let mut screen = footer_screen();
        let query = format!("{} {} {}", "工具".repeat(12), "e\u{301}".repeat(8), "👨‍👩‍👧");
        screen.start_search(&query);
        let state = screen.search.as_ref().expect("search state");
        let line = search_indicator_line(state, 30);
        let text = super::super::wrap::line_text(&line);

        assert!(text.contains('…'), "query cut was not disclosed: {text:?}");
        assert!(
            text.ends_with("\" ─ no matches"),
            "state rail or closing quote was lost: {text:?}"
        );
        assert!(display_width(&text) <= 30, "readout overflowed: {text:?}");
        let label = line.spans.last().expect("styled label");
        assert_eq!(label.style.fg, Some(crate::ui::palette::muted()));
        assert!(
            !label.style.add_modifier.contains(Modifier::DIM),
            "theme role replaces terminal DIM"
        );
    }

    #[test]
    fn follow_readout_uses_themed_muted_and_honest_narrow_fit() {
        let full = follow_indicator_line(42, 80);
        let label = full.spans.last().expect("styled label");
        assert_eq!(label.style.fg, Some(crate::ui::palette::muted()));
        assert!(!label.style.add_modifier.contains(Modifier::DIM));

        let narrow = follow_indicator_line(42, 8);
        let text = super::super::wrap::line_text(&narrow);
        assert_eq!(display_width(&text), 8);
        assert!(text.ends_with('…'), "narrow cut was silent: {text:?}");
    }

    /// Render composed lines through a real ratatui `Terminal<TestBackend>`
    /// and return the buffer rows as strings.
    fn frame_rows(lines: &[Line<'static>], width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| render_frame(frame, lines))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn compose_frame_strips_link_markers_and_resolves_a_click() {
        // An assistant message with a markdown link: after compose, the frame
        // cells carry NO escape markers (they cannot reach the ratatui Buffer),
        // and a click on the link's visible columns resolves to its target.
        let mut screen = footer_screen();
        screen.pager_active = true;
        screen.apply(UiEvent::AssistantTextEnd(
            "Read [guide](https://example.com/docs) now".to_string(),
        ));
        let size = Size::new(80, 24);
        let composed = compose_frame(&mut screen, size);

        // No frame line contains a link marker (clean cells for the Buffer).
        assert!(
            !composed
                .lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| { crate::ui::hyperlink::is_marker(s.content.as_ref()) }),
            "link markers leaked into the composed pager frame"
        );

        // A region was recorded, and a click inside it resolves to the target.
        let region = screen
            .pager_links
            .first()
            .expect("link region recorded")
            .clone();
        assert_eq!(region.uri, "https://example.com/docs");
        let hit = screen
            .pager_link_at(region.row as u16, region.start_col as u16)
            .expect("click resolves");
        assert_eq!(hit, "https://example.com/docs");
        // Just past the region is not a hit.
        assert!(
            screen
                .pager_link_at(region.row as u16, region.end_col as u16)
                .is_none(),
            "region end column is exclusive"
        );

        // The visible frame text still reads the label + destination verbatim.
        let rows = frame_rows(&composed.lines, 80, 24);
        assert!(
            rows.iter()
                .any(|r| r.contains("guide (https://example.com/docs)")),
            "visible link text unchanged"
        );
    }

    #[test]
    fn session_bar_stays_pinned_at_row_zero_through_a_10k_row_transcript() {
        let mut screen = footer_screen();
        for i in 0..10_000 {
            screen.apply(UiEvent::Notice(format!("row {i}")));
        }
        let size = Size::new(80, 24);
        let frame = compose_frame(&mut screen, size).lines;
        assert_eq!(frame.len(), 24, "frame is exactly the viewport height");
        let rows = frame_rows(&frame, 80, 24);
        assert!(
            rows[0].contains("~/repo") && rows[0].contains("CTX"),
            "session bar pinned at row 0: {:?}",
            rows[0]
        );
        // The transcript body under the bar shows the NEWEST rows (follow).
        let body = rows[2..].join("\n");
        assert!(body.contains("row 9999"), "follow view shows the tail");
        assert!(!body.contains("row 1 "), "oldest rows are scrolled out");
    }

    #[test]
    fn composer_chrome_is_pinned_at_the_frame_bottom() {
        let mut screen = footer_screen();
        screen.apply(UiEvent::Notice("hello".to_string()));
        let composed = compose_frame(&mut screen, Size::new(60, 20));
        // The focused composer emits the hardware-cursor marker: it must be
        // stripped from the cells and surfaced as a cursor position on the
        // editor input row.
        let cursor = composed.cursor.expect("focused composer yields a cursor");
        assert_eq!(cursor.1, 16, "cursor row is the editor input row");
        let frame = composed.lines;
        let rows = frame_rows(&frame, 60, 20);
        assert!(
            !rows.iter().any(|row| row.contains("iris:c")),
            "cursor marker never reaches the cells"
        );
        // Bottom padding row is blank; the statusline sits right above it and
        // carries the approval-policy segment.
        assert_eq!(rows[19].trim(), "");
        assert!(
            rows[18].contains("GPT-5.5") && rows[18].contains("\u{25c9}"),
            "composer statusline (mode glyph + model) at the bottom: {:?}",
            rows[18]
        );
    }

    #[test]
    fn start_page_renders_inside_the_pager_frame() {
        let mut screen = footer_screen();
        screen.show_start_page(0, true);
        let frame = compose_frame(&mut screen, Size::new(80, 30)).lines;
        assert_eq!(frame.len(), 30);
        let rows = frame_rows(&frame, 80, 30);
        let all = rows.join("\n");
        assert!(
            all.contains("Iris") || all.contains("iris"),
            "start page content present"
        );
    }

    #[test]
    fn follow_state_table_engage_disengage_overscroll() {
        let mut scroll = ScrollState::default();
        // 100 lines, 20-row view: max_top 80.
        scroll.sync(100, 20);
        assert!(scroll.is_following(), "initial state follows");
        assert_eq!(scroll.top(), 80);

        // Any upward scroll disengages and anchors.
        scroll.scroll_up(5);
        assert!(!scroll.is_following());
        assert_eq!(scroll.top(), 75);
        assert_eq!(scroll.lines_below(), 5);

        // Appends below do not move an anchored view (offset-from-top).
        scroll.sync(150, 20);
        assert_eq!(scroll.top(), 75);
        assert!(!scroll.is_following());

        // Scrolling down short of the bottom stays disengaged.
        scroll.scroll_down(10);
        assert!(!scroll.is_following());
        assert_eq!(scroll.top(), 85);

        // Overscrolling past the bottom re-engages follow.
        scroll.scroll_down(1000);
        assert!(scroll.is_following());
        assert_eq!(scroll.top(), 130);

        // Home jumps to the start (disengaged); End re-follows.
        scroll.jump_to_start();
        assert!(!scroll.is_following());
        assert_eq!(scroll.top(), 0);
        scroll.follow_latest();
        assert!(scroll.is_following());
    }

    #[test]
    fn scroll_clamps_and_re_follows_when_content_fits() {
        let mut scroll = ScrollState::default();
        scroll.sync(100, 20);
        scroll.scroll_up(1_000_000);
        assert_eq!(scroll.top(), 0, "scroll-up clamps at the start");
        scroll.page_down();
        assert_eq!(scroll.top(), 20);
        // A shrunken transcript (or grown viewport) with nothing to scroll
        // re-engages follow on the next layout sync.
        scroll.sync(10, 20);
        assert!(scroll.is_following());
        assert_eq!(scroll.top(), 0);
    }

    #[test]
    fn scrolled_frame_shows_history_and_follow_indicator() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        for i in 0..200 {
            screen.apply(UiEvent::Notice(format!("row {i}")));
        }
        // Warm the layout (sync populates the scroll metrics), then page up.
        let _ = compose_frame(&mut screen, Size::new(80, 24));
        assert!(screen.scroll.is_following());
        screen.scroll.page_up();
        screen.scroll.page_up();
        let frame = compose_frame(&mut screen, Size::new(80, 24)).lines;
        let rows = frame_rows(&frame, 80, 24);
        let body = rows[2..].join("\n");
        assert!(
            !body.contains("row 199"),
            "scrolled view no longer shows the tail"
        );
        assert!(
            body.contains("lines below"),
            "disengaged follow shows the dim indicator: {body}"
        );
        assert!(
            body.contains('\u{25be}'),
            "indicator carries the \u{25be} symbol"
        );
        // Scrolling far down re-engages follow and drops the indicator.
        screen.scroll.scroll_down(10_000);
        let frame = compose_frame(&mut screen, Size::new(80, 24)).lines;
        let rows = frame_rows(&frame, 80, 24);
        let body = rows[2..].join("\n");
        assert!(screen.scroll.is_following());
        assert!(body.contains("row 199"));
        assert!(!body.contains("lines below"));
    }

    #[test]
    fn resize_keeps_the_anchored_row_visible() {
        let mut screen = footer_screen();
        for i in 0..200 {
            screen.apply(UiEvent::Notice(format!("row {i}")));
        }
        let _ = compose_frame(&mut screen, Size::new(80, 24));
        screen.scroll.jump_to_start();
        let frame = compose_frame(&mut screen, Size::new(80, 24)).lines;
        let anchor = frame_rows(&frame, 80, 24)[2].clone();
        assert!(anchor.contains("row 0") || anchor.trim().is_empty());
        // Crossing the responsive floor removes the two-row top bar, but the
        // same transcript offset stays anchored at its new top edge.
        let frame = compose_frame(&mut screen, Size::new(80, 12)).lines;
        let rows = frame_rows(&frame, 80, 12);
        assert_eq!(rows[0], anchor, "anchor row survives a resize");
    }

    #[test]
    fn burst_streaming_and_finalize_preserve_an_anchored_scrollback_frame() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        screen.set_reduced_motion(true);
        for i in 0..240 {
            screen.apply(UiEvent::Notice(format!("history anchor {i:03}")));
        }
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "burst".to_string(),
        });
        screen.apply(UiEvent::AssistantTextDelta("warm stream\n".to_string()));
        let size = Size::new(80, 24);
        let _ = compose_frame(&mut screen, size);
        screen.scroll.page_up();
        screen.scroll.page_up();
        let before = compose_frame(&mut screen, size).lines;
        let bar_rows = session_bar_lines(&screen, 80, 24).len();
        let anchor = frame_rows(&before, 80, 24)[bar_rows].clone();
        let top = screen.scroll.top();
        assert!(anchor.contains("history anchor"), "{anchor:?}");
        assert!(!screen.scroll.is_following());

        // One event-loop interval can drain hundreds of provider chunks. None
        // may move a reader who explicitly left follow mode.
        for i in 0..600 {
            screen.apply(UiEvent::AssistantTextDelta(format!(
                "burst output {i:03}\n"
            )));
        }
        let during = compose_frame(&mut screen, size).lines;
        let during_rows = frame_rows(&during, 80, 24);
        assert_eq!(screen.scroll.top(), top, "burst changed the top offset");
        assert_eq!(
            during_rows[bar_rows], anchor,
            "burst streaming moved the anchored frame"
        );

        screen.apply(UiEvent::AssistantTextEnd(String::new()));
        let settled = compose_frame(&mut screen, size).lines;
        let settled_rows = frame_rows(&settled, 80, 24);
        assert_eq!(screen.scroll.top(), top, "finalize changed the top offset");
        assert_eq!(
            settled_rows[bar_rows], anchor,
            "stream finalization moved the anchored frame"
        );
    }

    #[test]
    fn minimum_frame_handles_unicode_shell_resize_cancel_and_focus() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        screen.set_reduced_motion(true);
        screen.start_turn();
        let call = ToolCall {
            id: "unicode-resize-shell".to_string(),
            thought_signature: None,
            name: "bash".to_string(),
            arguments: json!({
                "command": format!(
                    "printf '{} {}'",
                    "工具 e\u{301} 👨‍👩‍👧",
                    "界".repeat(96)
                )
            }),
        };
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: format!("FIRST 工具 e\u{301} 👨‍👩‍👧 {}\n", "界".repeat(120)),
        });

        let minimum = Size::new(80, 24);
        let before = compose_frame(&mut screen, minimum).lines;
        assert_eq!(before.len(), 24, "minimum frame is fully composed");
        assert!(
            before
                .iter()
                .all(|line| display_width(&super::super::wrap::line_text(line)) <= 80),
            "no logical row exceeds the minimum terminal width"
        );
        let _ = frame_rows(&before, 80, 24);

        // Resize while the tool is live, append at the new width, then return
        // to the declared minimum. The latest payload appears once and no
        // cluster degrades to a replacement scalar.
        let wide = compose_frame(&mut screen, Size::new(121, 31)).lines;
        assert_eq!(wide.len(), 31);
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "AFTER_RESIZE 工具 e\u{301} 👨‍👩‍👧\n".to_string(),
        });
        let back = compose_frame(&mut screen, minimum).lines;
        let back_logical = back
            .iter()
            .map(super::super::wrap::line_text)
            .collect::<Vec<_>>()
            .join("\n");
        let back_rows = frame_rows(&back, 80, 24);
        let back_text = back_rows.join("\n");
        assert_eq!(back_text.matches("AFTER_RESIZE").count(), 1, "{back_text}");
        assert!(
            back_logical.contains("工具"),
            "CJK survives resize: {back_logical}"
        );
        assert!(
            back_logical.contains("e\u{301}"),
            "combining cluster survives: {back_logical}"
        );
        assert!(
            !back_logical.contains('\u{fffd}'),
            "no torn Unicode: {back_logical}"
        );

        screen.apply(UiEvent::ToolCancelled(call));
        let cancelled = compose_frame(&mut screen, minimum).lines;
        let cancelled_text = frame_rows(&cancelled, 80, 24).join("\n");
        assert!(cancelled_text.contains("CANCELLED"), "{cancelled_text}");
        assert!(!cancelled_text.contains("RUNNING"), "{cancelled_text}");

        assert!(screen.toggle_scrollback_focus());
        let focused = compose_frame(&mut screen, minimum).lines;
        let focused_text = frame_rows(&focused, 80, 24).join("\n");
        assert!(
            focused_text.contains("SCROLLBACK"),
            "focus owner and key posture remain visible at 80x24: {focused_text}"
        );
    }

    #[test]
    fn frame_cost_is_independent_of_transcript_length() {
        use std::time::Instant;
        fn warm_compose_cost(rows: usize) -> std::time::Duration {
            let mut screen = footer_screen();
            for i in 0..rows {
                screen.apply(UiEvent::Notice(format!("row {i}")));
            }
            let size = Size::new(100, 40);
            // Warm the wrap cache; the measured frames must be pure window
            // clones + chrome.
            let _ = compose_frame(&mut screen, size);
            let start = Instant::now();
            for _ in 0..200 {
                let frame = compose_frame(&mut screen, size);
                assert_eq!(frame.lines.len(), 40);
            }
            start.elapsed()
        }
        let small = warm_compose_cost(500);
        let large = warm_compose_cost(10_000);
        // O(viewport) rendering: a 20x transcript must not cost 20x. The bound
        // is deliberately loose (8x) to absorb CI timing noise while still
        // failing any regression to whole-transcript rendering.
        assert!(
            large < small * 8,
            "10k-row frame cost {large:?} vs 500-row {small:?} suggests O(transcript) rendering"
        );
    }

    fn tool_call(id: usize) -> crate::nexus::ToolCall {
        crate::nexus::ToolCall {
            id: format!("call_{id}"),
            thought_signature: None,
            name: "bash".to_string(),
            arguments: serde_json::json!({ "command": "seq 40" }),
        }
    }

    /// Screen with `n` foldable tool panels (long output triggers the fold).
    fn screen_with_panels(n: usize) -> Screen {
        let mut screen = footer_screen();
        let long_output: String = (0..40).map(|i| format!("out {i}\n")).collect();
        for i in 0..n {
            screen.apply(UiEvent::ToolResult {
                call: tool_call(i),
                content: long_output.clone(),
                exit_code: Some(0),
                duration: None,
            });
        }
        screen
    }

    #[test]
    fn scrollback_focus_selects_folds_and_highlights_entries() {
        let mut screen = screen_with_panels(3);
        screen.pager_active = true;
        let _ = compose_frame(&mut screen, Size::new(80, 24));

        // Entering focus selects the newest entry.
        assert!(screen.toggle_scrollback_focus());
        let headers = screen.transcript.panel_header_rows();
        assert_eq!(headers.len(), 3);
        assert_eq!(screen.selected_entry, headers.last().copied());

        // Selection moves up/down and clamps at the ends.
        screen.move_selection(-1);
        assert_eq!(screen.selected_entry, Some(headers[1]));
        screen.move_selection(-1);
        screen.move_selection(-1);
        assert_eq!(screen.selected_entry, Some(headers[0]));
        screen.move_selection(1);
        assert_eq!(screen.selected_entry, Some(headers[1]));

        // Reveal/fold the selected block. These over-budget bodies arrive
        // collapsed (the flood guard); collapsed = header + footer only.
        assert!(screen.set_selected_expanded(true));
        assert!(!screen.set_selected_expanded(true), "already revealed");
        assert!(screen.toggle_selected_entry(), "toggle folds back");
        assert!(screen.set_selected_expanded(true), "and reveals again");

        // The selected header line carries the surface selection fill.
        let composed = compose_frame(&mut screen, Size::new(80, 24));
        let selected_line = screen
            .transcript_line_of_row(screen.selected_entry.expect("selected"))
            .expect("visible");
        let highlighted = composed.lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.style.bg == Some(crate::ui::palette::surface()))
        });
        assert!(
            highlighted,
            "selected entry header carries the surface fill"
        );
        assert!(selected_line < screen.transcript_visible_total(80));
    }

    #[test]
    fn reveal_scrolls_selection_into_view() {
        let mut scroll = ScrollState::default();
        scroll.sync(100, 20);
        assert!(scroll.is_following());
        // Selection far above: view scrolls up to it.
        scroll.reveal(10);
        assert!(!scroll.is_following());
        assert_eq!(scroll.top(), 10);
        // Selection below the view: minimal scroll down; at bottom re-follows.
        scroll.reveal(99);
        assert!(scroll.is_following());
        // Selection already visible: no movement.
        scroll.reveal(85);
        assert!(scroll.is_following());
    }

    #[test]
    fn find_jumps_to_newest_match_and_n_walks_older() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        for i in 0..100 {
            screen.apply(UiEvent::Notice(format!("row {i}")));
            if i % 10 == 0 {
                screen.apply(UiEvent::Notice(format!("needle {i}")));
            }
        }
        // Warm the wrap cache (search runs over it).
        let _ = compose_frame(&mut screen, Size::new(80, 24));

        let total = screen.start_search("NEEDLE").expect("search active");
        assert_eq!(total, 10, "case-insensitive matches");
        assert!(screen.scrollback_focus, "/find focuses the scrollback");
        let state = screen.search.as_ref().expect("state");
        assert_eq!(state.position, 10, "starts at the newest match");
        let newest = state.line.expect("line");

        // n walks older, clamping at the first match.
        assert!(screen.search_step(-1));
        assert!(screen.search.as_ref().unwrap().line.unwrap() < newest);
        for _ in 0..20 {
            let _ = screen.search_step(-1);
        }
        assert_eq!(screen.search.as_ref().unwrap().position, 1);
        // N walks newer, clamping at the newest.
        for _ in 0..20 {
            let _ = screen.search_step(1);
        }
        assert_eq!(screen.search.as_ref().unwrap().position, 10);

        // The compose shows the indicator and reveals + highlights the match.
        screen.search_step(0);
        let composed = compose_frame(&mut screen, Size::new(80, 24));
        let rows = frame_rows(&composed.lines, 80, 24);
        let body = rows[2..].join("\n");
        assert!(body.contains("10/10"), "indicator shows position: {body}");
        assert!(
            composed.lines.iter().any(|line| line
                .spans
                .iter()
                .any(|span| span.style.bg == Some(crate::ui::palette::surface()))),
            "current match carries the surface fill"
        );

        // Empty query clears the search and the indicator.
        assert!(screen.start_search("  ").is_none());
        assert!(screen.search.is_none());
        let composed = compose_frame(&mut screen, Size::new(80, 24));
        let all = frame_rows(&composed.lines, 80, 24).join("\n");
        assert!(!all.contains("find"), "indicator cleared: {all}");
    }

    #[test]
    fn find_matches_and_reveals_folded_panel_content() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        // A plain, always-visible match ABOVE the panel (oldest match).
        screen.apply(UiEvent::Notice("needle visible".to_string()));
        // A tool panel whose 40-line body collapses to a capped preview: the
        // full body (and most matches) is folded away.
        let long_output: String = (0..40).map(|i| format!("needle body {i}\n")).collect();
        screen.apply(UiEvent::ToolResult {
            call: tool_call(0),
            content: long_output,
            exit_code: Some(0),
            duration: None,
        });
        let _ = compose_frame(&mut screen, Size::new(80, 24));
        assert!(
            screen.transcript.latest_panel_collapsed(),
            "the tool panel starts collapsed"
        );

        // A match hidden inside the collapsed body is found. "needle body 20"
        // is a single middle line -- searched despite being folded away, and
        // NOT double-counted against the collapsed head/tail preview.
        let total = screen
            .start_search("needle body 20")
            .expect("search active");
        assert_eq!(total, 1, "exactly one hidden body line matches");
        let line = screen.search.as_ref().unwrap().line;
        assert!(
            line.is_some(),
            "the folded match resolves to a visible line"
        );
        assert!(
            !screen.transcript.latest_panel_collapsed(),
            "jumping into the fold expands the panel so the match is visible"
        );
        assert!(
            line.unwrap() < screen.transcript_visible_total(80),
            "the revealed match sits within the visible transcript"
        );

        // Count includes every hidden match: the plain notice plus all 40 body
        // lines (preview duplicates excluded), never the head/tail preview.
        let total = screen.start_search("needle").expect("search active");
        assert_eq!(total, 41, "1 visible notice + 40 folded body lines");
        let state = screen.search.as_ref().unwrap();
        assert_eq!(state.position, 41, "starts at the newest (deepest) match");

        // n walks older across folded then unfolded matches, clamping at the
        // oldest (the plain notice).
        for _ in 0..60 {
            let _ = screen.search_step(-1);
        }
        assert_eq!(screen.search.as_ref().unwrap().position, 1);
        let composed = compose_frame(&mut screen, Size::new(80, 24));
        let body = frame_rows(&composed.lines, 80, 24)[2..].join("\n");
        assert!(
            body.contains("needle visible"),
            "oldest match is the visible notice: {body}"
        );
        // N walks newer back into the folded body, clamping at the newest.
        for _ in 0..60 {
            let _ = screen.search_step(1);
        }
        assert_eq!(screen.search.as_ref().unwrap().position, 41);
        let composed = compose_frame(&mut screen, Size::new(80, 24));
        assert!(
            composed.lines.iter().any(|line| line
                .spans
                .iter()
                .any(|span| span.style.bg == Some(crate::ui::palette::surface()))),
            "current folded match carries the surface highlight"
        );

        // Empty query still clears the search.
        assert!(screen.start_search("   ").is_none());
        assert!(screen.search.is_none());
    }

    #[test]
    fn search_with_no_matches_reports_zero_and_jumps_nowhere() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        screen.apply(UiEvent::Notice("hello".to_string()));
        let _ = compose_frame(&mut screen, Size::new(80, 24));
        assert_eq!(screen.start_search("absent"), Some(0));
        let state = screen.search.as_ref().expect("state");
        assert_eq!(state.total, 0);
        assert!(state.line.is_none());
        assert!(!screen.search_step(-1));
    }

    #[test]
    fn find_does_not_match_fold_affordance_chrome() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        // A tool panel whose long body folds, rendering both fold affordance
        // hints (`ctrl+o to expand`/`collapse`). None of the real output text
        // contains "ctrl", "collapse" or "to expand".
        let long_output: String = (0..40).map(|i| format!("body line {i}\n")).collect();
        screen.apply(UiEvent::ToolResult {
            call: tool_call(0),
            content: long_output,
            exit_code: Some(0),
            duration: None,
        });
        let _ = compose_frame(&mut screen, Size::new(80, 24));

        // The affordance rows carry `ctrl+o to expand`/`collapse` text but are
        // control chrome, not transcript content: `/find` must skip them so a
        // query never matches hidden UI or auto-expands the panel for it.
        assert_eq!(
            screen.start_search("ctrl+o"),
            Some(0),
            "fold affordance chrome is not searched"
        );
        assert!(
            screen.search.as_ref().unwrap().line.is_none(),
            "no chrome match means no jump"
        );
        assert!(
            screen.transcript.latest_panel_collapsed(),
            "a non-matching chrome query leaves the panel collapsed"
        );
        assert_eq!(screen.start_search("collapse"), Some(0));
        assert_eq!(screen.start_search("to expand"), Some(0));
        // Sanity: real folded body content is still matched.
        assert_eq!(
            screen.start_search("body line 20"),
            Some(1),
            "real transcript content is still searchable"
        );
    }

    #[test]
    fn search_reveal_lifts_the_final_line_above_the_indicator() {
        // Without a reserved indicator row, revealing the final line pins it to
        // the last body row (row 19) -- the row compose overwrites with the
        // search indicator, hiding the match.
        let mut scroll = ScrollState::default();
        scroll.sync(100, 20);
        scroll.jump_to_start();
        scroll.reveal(99);
        assert_eq!(scroll.top(), 80);
        assert_eq!(99 - scroll.top(), 19, "plain reveal lands on the last row");

        // Reserving the indicator row extends the scrollable range by one, so
        // the same final-line match lands on row 18 -- one above the indicator.
        let mut scroll = ScrollState::default();
        scroll.set_bottom_pad(1);
        scroll.sync(100, 20);
        scroll.jump_to_start();
        scroll.reveal_with_bottom_margin(99, 1);
        assert!(scroll.is_following(), "an EOF reveal re-engages follow");
        assert_eq!(scroll.top(), 81);
        assert_eq!(
            99 - scroll.top(),
            18,
            "final-line match sits above the reserved indicator row"
        );
    }

    #[test]
    fn final_line_match_stays_visible_above_the_search_indicator() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        for i in 0..80 {
            screen.apply(UiEvent::Notice(format!("filler {i}")));
        }
        // The only match is on the very LAST transcript line.
        screen.apply(UiEvent::Notice("needle tail".to_string()));
        let size = Size::new(80, 24);
        let _ = compose_frame(&mut screen, size);
        assert_eq!(screen.start_search("needle tail"), Some(1));
        // Compose reveals + highlights the match.
        let frame = compose_frame(&mut screen, size).lines;
        let rows = frame_rows(&frame, 80, 24);
        // The centered indicator draws its `k/N` on the last body row. The
        // highlighted match must render on an EARLIER row, not under it.
        let indicator_row = rows
            .iter()
            .position(|row| row.contains("1/1"))
            .expect("indicator rendered");
        let match_row = frame
            .iter()
            .position(|line| {
                line.spans
                    .iter()
                    .any(|span| span.style.bg == Some(crate::ui::palette::surface()))
            })
            .expect("match highlighted");
        assert!(
            match_row < indicator_row,
            "final-line match sits above the indicator: match@{match_row} indicator@{indicator_row}"
        );
        assert!(
            rows[match_row].contains("needle tail"),
            "the highlighted row is the match: {:?}",
            rows[match_row]
        );
    }

    #[test]
    fn sticky_prompt_band_pins_the_newest_prompt_scrolled_past() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        screen.commit_user("first question about apples");
        for i in 0..60 {
            screen.apply(UiEvent::Notice(format!("answer detail {i}")));
        }
        screen.commit_user("second question about oranges");
        for i in 0..60 {
            screen.apply(UiEvent::Notice(format!("more detail {i}")));
        }
        let size = Size::new(80, 24);
        // Following at the bottom: the second prompt has scrolled past the
        // top, so it is pinned as a dim band under the session bar.
        let frame = compose_frame(&mut screen, size).lines;
        let rows = frame_rows(&frame, 80, 24);
        assert!(
            rows[2].starts_with("    \u{25b8} \u{203a} second question about oranges"),
            "collapsed band pins the governing prompt in its compact row: {:?}",
            rows[2]
        );
        assert_eq!(
            rows[3], rows[1],
            "collapsed job card needs the same quiet closing hairline as its open posture: \
             band {:?} vs bar {:?}",
            rows[3], rows[1]
        );
        assert!(
            screen.toggle_sticky_prompt(),
            "band toggle target is available"
        );
        let frame = compose_frame(&mut screen, size).lines;
        let rows = frame_rows(&frame, 80, 24);
        assert!(
            rows[2].starts_with("    \u{25be} \u{203a} second question about oranges"),
            "expanded band shows the disclosure and prompt: {:?}",
            rows[2]
        );
        // The expanded band closes with the session bar's OWN hairline: inset dim
        // `─`, never the full-width composer border. rows[1] is the session bar rule.
        assert_eq!(
            rows[3], rows[1],
            "band rule is byte-identical to the session bar rule (inset, dim, \
             not a full-width border): band {:?} vs bar {:?}",
            rows[3], rows[1]
        );
        assert!(
            rows[3].starts_with("  \u{2500}") && !rows[3].starts_with('\u{2500}'),
            "band rule is inset by the box padding, not full-width: {:?}",
            rows[3]
        );

        // Scrolled into the first answer: the FIRST prompt is the sticky one.
        screen.scroll.jump_to_start();
        screen.scroll.scroll_down(10);
        let frame = compose_frame(&mut screen, size).lines;
        let rows = frame_rows(&frame, 80, 24);
        assert!(
            rows[2].contains("\u{203a} first question about apples"),
            "older region pins the older prompt: {:?}",
            rows[2]
        );

        // At the very top nothing has scrolled past: no sticky overlay (the
        // first prompt is simply the first content row, and the newer prompt
        // is not pinned over it).
        screen.scroll.jump_to_start();
        let frame = compose_frame(&mut screen, size).lines;
        let rows = frame_rows(&frame, 80, 24);
        assert!(
            !rows[2].contains("second question"),
            "no sticky at the top: {:?}",
            rows[2]
        );
    }

    #[test]
    fn sticky_prompt_band_wraps_prompt_with_continuation_indent() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        screen.commit_user(
            "We have symbols and glyphs defined in our design system / language. Currently the \
             footer of tool output shows DONE RUNNING ERROR.",
        );
        for i in 0..80 {
            screen.apply(UiEvent::Notice(format!("detail {i}")));
        }
        let _ = compose_frame(&mut screen, Size::new(72, 24));
        assert!(screen.toggle_sticky_prompt());
        let frame = compose_frame(&mut screen, Size::new(72, 24)).lines;
        let rows = frame_rows(&frame, 72, 24);
        // Header row on the transcript column with a disclosure and `›` marker.
        assert!(
            rows[2].starts_with("    \u{25be} \u{203a} We have symbols"),
            "first band row has the disclosure and user marker: {:?}",
            rows[2]
        );
        // Continuation hangs under the disclosure+marker text, unmarked.
        assert!(
            rows[3].starts_with("        ") && !rows[3].contains('\u{203a}'),
            "wrapped continuation hangs under the body without the marker: {:?}",
            rows[3]
        );
    }

    #[test]
    fn sticky_prompt_band_collapses_long_prompt_to_header_and_rule() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        let long = "word ".repeat(120);
        screen.commit_user(long.trim());
        for i in 0..80 {
            screen.apply(UiEvent::Notice(format!("detail {i}")));
        }
        let frame = compose_frame(&mut screen, Size::new(72, 24)).lines;
        let rows = frame_rows(&frame, 72, 24);
        assert!(
            rows[2].starts_with("    \u{25b8} \u{203a} word word"),
            "collapsed prompt has one disclosure row: {:?}",
            rows[2]
        );
        assert_eq!(
            rows[3], rows[1],
            "the second row is the stable closing hairline: {:?}",
            rows[3]
        );

        assert!(screen.toggle_sticky_prompt());
        let frame = compose_frame(&mut screen, Size::new(72, 24)).lines;
        let rows = frame_rows(&frame, 72, 24);
        assert!(
            rows[3].contains("word word"),
            "expanded prompt reveals continuation rows: {:?}",
            rows[3]
        );
    }

    #[test]
    fn clipped_expanded_sticky_band_reports_hidden_rows_and_keeps_its_rule() {
        let prompt = "precision instrument ".repeat(80);
        let band = sticky_prompt_band(prompt.trim(), 48, 5, true);
        let rows: Vec<String> = band.iter().map(super::super::wrap::line_text).collect();

        assert_eq!(rows.len(), 5, "band obeys its exact viewport budget");
        assert!(rows[0].contains("precision instrument"), "{rows:?}");
        assert!(
            rows[3].contains('…') && rows[3].contains("rows"),
            "penultimate row honestly reports clipped prompt rows: {rows:?}"
        );
        assert!(
            rows[4].trim().chars().all(|cell| cell == '─'),
            "the stable closing hairline survives clipping: {rows:?}"
        );
    }

    #[test]
    fn collapsed_band_paints_ink_text_and_a_muted_plus_n_marker() {
        // A prompt long enough to wrap hides rows when collapsed: the opening
        // line renders in body ink, the chrome stays muted-bold, and a dim
        // right-aligned `+N` reports the hidden rows (job-card honesty).
        let mut screen = footer_screen();
        screen.pager_active = true;
        let long = "word ".repeat(120);
        screen.commit_user(long.trim());
        for i in 0..80 {
            screen.apply(UiEvent::Notice(format!("detail {i}")));
        }
        let size = Size::new(72, 24);
        let frame = compose_frame(&mut screen, size).lines;
        let band = &frame[2];

        // The opening line is body ink (`panel_style`), not dim.
        assert!(
            band.spans
                .iter()
                .any(|s| s.style == panel_style() && s.content.contains("word")),
            "the prompt's opening line is body ink: {:?}",
            band.spans
        );
        // The disclosure + user chrome stays muted bold.
        assert!(
            band.spans.iter().any(|s| s.content.contains('\u{25b8}')
                && s.style == dim_style().add_modifier(Modifier::BOLD)),
            "the disclosure arrow is muted-bold chrome: {:?}",
            band.spans
        );
        // A right-aligned dim `+N` marker counts the hidden rows.
        let marker = band
            .spans
            .iter()
            .find(|s| s.content.starts_with('+'))
            .expect("a +N marker span when rows are hidden");
        assert_eq!(marker.style, dim_style(), "the +N marker is dim/muted");
        let n: usize = marker.content[1..].parse().expect("+N is a count");
        assert!(n > 0, "the marker reports a positive hidden-row count");
        // The truncated opening carries an ellipsis so ink never touches `+N`.
        let text = frame_rows(&frame, 72, 24)[2].clone();
        assert!(
            text.contains('\u{2026}') && text.trim_end().ends_with(marker.content.as_ref()),
            "collapsed row is `…` text then a right-aligned `+N`: {text:?}"
        );
    }

    #[test]
    fn collapsed_band_omits_the_marker_when_the_prompt_fits_one_row() {
        // A prompt that fits a single row hides nothing: no `+N`, ink text.
        let mut screen = footer_screen();
        screen.pager_active = true;
        screen.commit_user("a short governing prompt");
        for i in 0..40 {
            screen.apply(UiEvent::Notice(format!("detail {i}")));
        }
        let size = Size::new(80, 24);
        let frame = compose_frame(&mut screen, size).lines;
        let band = &frame[2];
        assert!(
            band.spans.iter().all(|s| !s.content.starts_with('+')),
            "no marker when the prompt fits one row: {:?}",
            band.spans
        );
        assert!(
            band.spans
                .iter()
                .any(|s| s.style == panel_style() && s.content.contains("short governing prompt")),
            "the whole prompt renders in body ink: {:?}",
            band.spans
        );
    }

    #[test]
    fn sticky_header_yields_to_a_search_match_at_the_viewport_top() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        screen.commit_user("question about pears");
        for i in 0..30 {
            screen.apply(UiEvent::Notice(format!("filler {i}")));
        }
        screen.apply(UiEvent::Notice("needle here".to_string()));
        for i in 0..60 {
            screen.apply(UiEvent::Notice(format!("tail {i}")));
        }
        let size = Size::new(80, 24);
        let _ = compose_frame(&mut screen, size);
        assert_eq!(screen.start_search("needle"), Some(1));
        // First compose reveals the match; force it to the exact viewport top
        // and re-compose.
        let _ = compose_frame(&mut screen, size);
        let match_line = screen.search.as_ref().unwrap().line.expect("line");
        screen.scroll.jump_to_start();
        screen.scroll.scroll_down(match_line);
        let frame = compose_frame(&mut screen, size).lines;
        let rows = frame_rows(&frame, 80, 24);
        assert!(
            rows[2].contains("needle here"),
            "match at the top keeps its row: {:?}",
            rows[2]
        );
        assert!(
            frame[2]
                .spans
                .iter()
                .any(|span| span.style.bg == Some(crate::ui::palette::surface())),
            "match at the top keeps its highlight"
        );
    }

    #[test]
    fn expanded_sticky_band_yields_to_a_match_anywhere_under_its_footprint() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        screen.commit_user("question about pears");
        for i in 0..30 {
            screen.apply(UiEvent::Notice(format!("filler {i}")));
        }
        screen.apply(UiEvent::Notice("needle under the rule".to_string()));
        for i in 0..60 {
            screen.apply(UiEvent::Notice(format!("tail {i}")));
        }
        let size = Size::new(80, 24);
        let _ = compose_frame(&mut screen, size);
        assert_eq!(screen.start_search("needle under the rule"), Some(1));
        let _ = compose_frame(&mut screen, size);
        let match_line = screen.search.as_ref().unwrap().line.expect("line");
        assert!(match_line > 0);

        // Expanded one-line card occupies body rows 0..2 (prompt + rule). Put
        // the active match on row 1: the old exact-top gate missed it and the
        // closing rule painted over the highlight.
        screen.sticky_prompt_expanded = true;
        screen.scroll.jump_to_start();
        screen.scroll.scroll_down(match_line - 1);
        let frame = compose_frame(&mut screen, size).lines;
        let rows = frame_rows(&frame, 80, 24);
        assert!(rows[3].contains("needle under the rule"), "{:?}", rows[3]);
        assert!(
            frame[3]
                .spans
                .iter()
                .any(|span| span.style.bg == Some(crate::ui::palette::surface())),
            "match under the band keeps its highlight"
        );
        assert!(
            !rows[2].contains("question about pears"),
            "the whole sticky band yields as one stable overlay"
        );
    }

    #[test]
    fn composed_hit_map_rejects_a_header_hidden_under_the_composer() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        shell_history(&mut screen, 32);
        let size = Size::new(80, 24);
        let warm = compose_frame(&mut screen, size);
        let rows = frame_rows(&warm.lines, 80, 24);
        let bar_rows = session_bar_lines(&screen, 80, 24).len();
        let composer_row = rows
            .iter()
            .position(|row| row.contains("Give Iris a task"))
            .expect("composer row");
        let body_offset = composer_row - bar_rows;
        let header = align_header_under(&mut screen, size, body_offset, false);
        let frame = compose_frame(&mut screen, size);
        let rows = frame_rows(&frame.lines, 80, 24);

        assert!(rows[composer_row].contains("Give Iris a task"));
        assert_eq!(
            screen
                .transcript
                .header_row_at_visible_line(screen.scroll.top() + body_offset),
            Some(header),
            "the old arithmetic target is deliberately a real hidden header"
        );
        let before = screen.transcript.panel_expanded_at(header);
        assert!(
            !screen.toggle_header_at_screen_row(composer_row as u16),
            "pinned composer chrome owns its row"
        );
        assert_eq!(screen.transcript.panel_expanded_at(header), before);
    }

    #[test]
    fn composed_hit_map_rejects_headers_under_search_and_follow_readouts() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        shell_history(&mut screen, 36);
        let size = Size::new(80, 24);
        assert_eq!(screen.start_search("no-such-result"), Some(0));
        screen.focus_prompt();
        let warm = compose_frame(&mut screen, size);
        let rows = frame_rows(&warm.lines, 80, 24);
        let bar_rows = session_bar_lines(&screen, 80, 24).len();
        let search_row = rows
            .iter()
            .position(|row| row.contains("no matches"))
            .expect("search readout");
        let body_offset = search_row - bar_rows;
        let search_header = align_header_under(&mut screen, size, body_offset, false);
        let frame = compose_frame(&mut screen, size);
        let rows = frame_rows(&frame.lines, 80, 24);
        assert!(rows[search_row].contains("no matches"));
        assert_eq!(
            screen
                .transcript
                .header_row_at_visible_line(screen.scroll.top() + body_offset),
            Some(search_header)
        );
        let before = screen.transcript.panel_expanded_at(search_header);
        assert!(!screen.toggle_header_at_screen_row(search_row as u16));
        assert_eq!(screen.transcript.panel_expanded_at(search_header), before);

        screen.start_search("");
        let follow_header = align_header_under(&mut screen, size, body_offset, true);
        let frame = compose_frame(&mut screen, size);
        let rows = frame_rows(&frame.lines, 80, 24);
        assert!(
            rows[search_row].contains("lines below"),
            "follow readout replaced the same body row: {:?}",
            rows[search_row]
        );
        assert_eq!(
            screen
                .transcript
                .header_row_at_visible_line(screen.scroll.top() + body_offset),
            Some(follow_header)
        );
        let before = screen.transcript.panel_expanded_at(follow_header);
        assert!(!screen.toggle_header_at_screen_row(search_row as u16));
        assert_eq!(screen.transcript.panel_expanded_at(follow_header), before);
    }

    #[test]
    fn sticky_continuation_owns_its_hit_region_instead_of_hidden_headers() {
        let mut screen = footer_screen();
        screen.pager_active = true;
        screen.commit_user(&"governing prompt ".repeat(30));
        shell_history(&mut screen, 28);
        screen.sticky_prompt_expanded = true;
        let size = Size::new(80, 24);
        let bar_rows = session_bar_lines(&screen, 80, 24).len();
        let header = align_header_under(&mut screen, size, 1, true);
        assert!(screen.scroll.top() > 0, "a sticky prompt is pinned");
        let frame = compose_frame(&mut screen, size);
        let rows = frame_rows(&frame.lines, 80, 24);
        let continuation_row = bar_rows + 1;

        assert!(
            rows[continuation_row].contains("governing prompt"),
            "expanded continuation covers the transcript row: {:?}",
            rows[continuation_row]
        );
        assert_eq!(
            screen
                .transcript
                .header_row_at_visible_line(screen.scroll.top() + 1),
            Some(header),
            "a real foldable header is hidden under the continuation"
        );
        let before = screen.transcript.panel_expanded_at(header);
        assert!(
            !screen.toggle_header_at_screen_row(continuation_row as u16),
            "the continuation consumes its own hit region"
        );
        assert_eq!(screen.transcript.panel_expanded_at(header), before);
    }

    #[test]
    fn width_height_sweep_never_overflows_or_panics() {
        for &width in &[1u16, 2, 10, 40, 80, 121] {
            for &height in &[1u16, 2, 5, 24, 50] {
                let mut screen = footer_screen();
                for i in 0..50 {
                    screen.apply(UiEvent::Notice(format!("line {i}")));
                }
                let frame = compose_frame(&mut screen, Size::new(width, height)).lines;
                assert!(
                    frame.len() <= usize::from(height),
                    "{width}x{height}: frame must fit the viewport"
                );
                // Rendering through a real terminal asserts no cell overflow.
                let _ = frame_rows(&frame, width, height);
            }
        }
    }

    #[test]
    fn enter_and_leave_emit_the_golden_sequences() {
        let _guard = lock();
        let mut surface = AltScreen::enter(Vec::new()).expect("enter");
        assert_eq!(surface.writer, b"\x1b[?1049h\x1b[2J\x1b[1;1H");
        assert!(crate::signals::alt_screen_active());
        surface.writer.clear();
        surface.leave().expect("leave");
        assert_eq!(surface.writer, b"\x1b[?1049l");
        assert!(!crate::signals::alt_screen_active());
    }

    #[test]
    fn leave_is_idempotent_and_drop_emits_it_once() {
        let _guard = lock();
        let mut surface = AltScreen::enter(Vec::new()).expect("enter");
        surface.leave().expect("leave");
        surface.writer.clear();
        surface.leave().expect("second leave");
        assert!(surface.writer.is_empty(), "second leave must be a no-op");

        // Drop after an explicit leave adds nothing; Drop without one leaves.
        let surface = AltScreen::enter(Vec::new()).expect("enter");
        drop(surface);
        assert!(!crate::signals::alt_screen_active());
    }

    #[test]
    fn emergency_restore_leaves_alt_screen_only_while_active() {
        let _guard = lock();
        crate::signals::set_alt_screen_active(false);
        let mut idle = Vec::new();
        emergency_restore(&mut idle).expect("restore");
        assert!(idle.is_empty(), "inactive pager must not write anything");

        crate::signals::set_alt_screen_active(true);
        let mut out = Vec::new();
        emergency_restore(&mut out).expect("restore");
        assert_eq!(out, emergency_restore_bytes());
        assert!(!crate::signals::alt_screen_active());

        // A second emergency restore (hook then Drop racing) is a no-op.
        let mut again = Vec::new();
        emergency_restore(&mut again).expect("restore");
        assert!(again.is_empty());
    }

    #[test]
    fn drop_after_panic_hook_restore_does_not_double_emit() {
        let _guard = lock();
        let mut surface = AltScreen::enter(Vec::new()).expect("enter");
        // Simulate the panic hook having already restored the screen.
        let mut hook_out = Vec::new();
        emergency_restore(&mut hook_out).expect("restore");
        assert_eq!(hook_out, emergency_restore_bytes());
        surface.writer.clear();
        surface.leave().expect("leave");
        assert!(
            surface.writer.is_empty(),
            "leave after the hook restored must not re-emit ?1049l"
        );
    }
}
