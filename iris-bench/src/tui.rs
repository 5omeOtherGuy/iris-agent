//! Live run TUI — a single quiet column that tracks the Iris design language
//! (`docs/TUI_DESIGN_LANGUAGE.md`): a session-bar-style header with a 10-dot LED
//! progress meter, a symbol+label+color count line, a structured matrix of the
//! run (grouped model → workload, with a compact fallback), a rolling
//! recent-activity log, and an inline LED-chase working indicator that reports
//! the live, honestly-paired defaults-vs-baseline token delta — the one number
//! this tool exists to measure.
//!
//! The engine runs on a background thread; this thread drains its event stream,
//! renders at ~10fps, and cancels the run on `q`/`Esc`/`Ctrl-C`. Color and
//! motion are point signals paired with a glyph, never a fill or a spinner —
//! and both fold away under `NO_COLOR` / `IRIS_REDUCED_MOTION` (see `style`).

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tokio_util::sync::CancellationToken;

use crate::engine::{self, Summary};
use crate::event::CellEvent;
use crate::record::CellRecord;
use crate::spec::{Cell, RunSpec};
use crate::style::{self, Prefs};
use crate::workloads::WorkloadSpec;

const RECENT_CAP: usize = 24;
const RECENT_SHOWN: usize = 6;
const METER_DOTS: usize = 10;
const WL_LABEL_MAX: usize = 18;
const CHASE_STEP_MS: u128 = 130;
/// Below this terminal height the recent-activity panel is dropped so the grid
/// keeps its room.
const RECENT_MIN_HEIGHT: u16 = 18;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending,
    Running,
    Success,
    CheckFailed,
    Error,
}

impl Status {
    /// The state glyph, role color, and label — symbol first so the grid is
    /// legible in monochrome (§12). Running and warning share the accent hue but
    /// never the shape.
    fn mark(self) -> (&'static str, ratatui::style::Color, &'static str) {
        match self {
            Status::Pending => (style::QUEUED, style::MUTED, "pending"),
            Status::Running => (style::RUNNING, style::ACCENT, "running"),
            Status::Success => (style::DONE, style::SUCCESS, "ok"),
            Status::CheckFailed => (style::WARN, style::ACCENT, "check-fail"),
            Status::Error => (style::ERROR, style::DANGER, "err"),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct CellState {
    status_i: u8,
}

impl CellState {
    fn status(self) -> Status {
        match self.status_i {
            1 => Status::Running,
            2 => Status::Success,
            3 => Status::CheckFailed,
            4 => Status::Error,
            _ => Status::Pending,
        }
    }
    fn set(&mut self, s: Status) {
        self.status_i = match s {
            Status::Pending => 0,
            Status::Running => 1,
            Status::Success => 2,
            Status::CheckFailed => 3,
            Status::Error => 4,
        };
    }
}

/// One settled cell, newest-first, for the recent-activity log.
struct Recent {
    status: Status,
    model: String,
    workload: String,
    arm: String,
    run: usize,
    turns: u32,
    input_tokens: u64,
    note: String,
}

/// Live, honestly-paired defaults-vs-baseline input-token accumulator. A
/// `(model, workload)` group contributes to the delta only once BOTH arms have
/// a valid cell — mirroring `analysis::Group::is_paired` — so the headline
/// number is never asserted before it is measured.
#[derive(Default)]
struct Delta {
    groups: HashMap<(String, String), [ArmAcc; 2]>, // [baseline, defaults]
}

#[derive(Default, Clone, Copy)]
struct ArmAcc {
    sum: u64,
    n: u32,
}

impl Delta {
    fn observe(&mut self, rec: &CellRecord) {
        if !rec.valid {
            return;
        }
        let slot = match rec.arm.as_str() {
            "baseline" => 0,
            "defaults" => 1,
            _ => return,
        };
        let acc = &mut self
            .groups
            .entry((rec.model.clone(), rec.workload.clone()))
            .or_default()[slot];
        acc.sum += rec.input_tokens;
        acc.n += 1;
    }

    /// Mean paired delta % (negative = defaults spends fewer input tokens) and
    /// the number of complete pairs backing it, or `None` until one pair exists.
    fn summary(&self) -> Option<(f64, usize)> {
        let mut total = 0.0;
        let mut pairs = 0usize;
        for [base, def] in self.groups.values() {
            if base.n > 0 && def.n > 0 {
                let b = base.sum as f64 / base.n as f64;
                let d = def.sum as f64 / def.n as f64;
                if b > 0.0 {
                    total += 100.0 * (d - b) / b;
                    pairs += 1;
                }
            }
        }
        (pairs > 0).then(|| (total / pairs as f64, pairs))
    }
}

/// Everything a frame needs — a plain value so `render` is a pure function of
/// it and can be exercised against a `TestBackend`.
struct View<'a> {
    spec: &'a RunSpec,
    cells: &'a [Cell],
    states: &'a [CellState],
    recent: &'a VecDeque<Recent>,
    delta: Option<(f64, usize)>,
    elapsed: Duration,
    cancelling: bool,
    prefs: Prefs,
}

/// Drive a run with the live TUI; returns the run summary.
pub fn run_live(spec: &RunSpec, catalog: &[WorkloadSpec]) -> anyhow::Result<Summary> {
    let cells = spec.expand();
    let total = cells.len();
    let mut states = vec![CellState::default(); total];
    let mut recent: VecDeque<Recent> = VecDeque::new();
    let mut delta = Delta::default();
    let prefs = Prefs::from_env();
    let start = Instant::now();

    let (tx, rx) = mpsc::channel::<CellEvent>();
    let cancel = CancellationToken::new();

    let spec_c = spec.clone();
    let catalog_c = catalog.to_vec();
    let cancel_c = cancel.clone();
    let engine_handle = thread::spawn(move || {
        engine::run(&spec_c, &catalog_c, &cancel_c, |ev| {
            let _ = tx.send(ev);
        })
    });

    let mut terminal = ratatui::init();
    let draw_result = (|| -> anyhow::Result<()> {
        loop {
            while let Ok(ev) = rx.try_recv() {
                apply(ev, &mut states, &mut recent, &mut delta);
            }
            let view = View {
                spec,
                cells: &cells,
                states: &states,
                recent: &recent,
                delta: delta.summary(),
                elapsed: start.elapsed(),
                cancelling: cancel.is_cancelled(),
                prefs,
            };
            terminal.draw(|f| render(f, &view))?;

            if event::poll(Duration::from_millis(100))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                let quit = matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                    || (k.code == KeyCode::Char('c')
                        && k.modifiers.contains(KeyModifiers::CONTROL));
                if quit {
                    cancel.cancel();
                }
            }

            if engine_handle.is_finished() {
                while let Ok(ev) = rx.try_recv() {
                    apply(ev, &mut states, &mut recent, &mut delta);
                }
                let view = View {
                    spec,
                    cells: &cells,
                    states: &states,
                    recent: &recent,
                    delta: delta.summary(),
                    elapsed: start.elapsed(),
                    cancelling: cancel.is_cancelled(),
                    prefs,
                };
                terminal.draw(|f| render(f, &view))?;
                break;
            }
        }
        Ok(())
    })();
    ratatui::restore();
    draw_result?;

    match engine_handle.join() {
        Ok(res) => Ok(res?),
        Err(_) => anyhow::bail!("engine thread panicked"),
    }
}

fn apply(
    ev: CellEvent,
    states: &mut [CellState],
    recent: &mut VecDeque<Recent>,
    delta: &mut Delta,
) {
    match ev {
        CellEvent::Started { index, .. } => {
            if let Some(s) = states.get_mut(index) {
                s.set(Status::Running);
            }
        }
        CellEvent::Finished { index, record } => {
            let status = if record.success {
                Status::Success
            } else {
                Status::CheckFailed
            };
            if let Some(s) = states.get_mut(index) {
                s.set(status);
            }
            delta.observe(&record);
            push_recent(
                recent,
                Recent {
                    status,
                    model: record.model.clone(),
                    workload: record.workload.clone(),
                    arm: record.arm.clone(),
                    run: record.run,
                    turns: record.turns,
                    input_tokens: record.input_tokens,
                    note: String::new(),
                },
            );
        }
        CellEvent::Failed {
            index,
            cell,
            reason,
        } => {
            if let Some(s) = states.get_mut(index) {
                s.set(Status::Error);
            }
            push_recent(
                recent,
                Recent {
                    status: Status::Error,
                    model: cell.model,
                    workload: cell.workload,
                    arm: cell.arm,
                    run: cell.run,
                    turns: 0,
                    input_tokens: 0,
                    note: reason,
                },
            );
        }
    }
}

fn push_recent(recent: &mut VecDeque<Recent>, entry: Recent) {
    recent.push_front(entry);
    while recent.len() > RECENT_CAP {
        recent.pop_back();
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Default)]
struct Counts {
    running: usize,
    success: usize,
    check_failed: usize,
    errored: usize,
    pending: usize,
    total: usize,
}

impl Counts {
    fn done(self) -> usize {
        self.success + self.check_failed + self.errored
    }
    fn all_settled(self) -> bool {
        self.total > 0 && self.done() == self.total
    }
}

fn tally(states: &[CellState]) -> Counts {
    let mut c = Counts {
        total: states.len(),
        ..Counts::default()
    };
    for s in states {
        match s.status() {
            Status::Pending => c.pending += 1,
            Status::Running => c.running += 1,
            Status::Success => c.success += 1,
            Status::CheckFailed => c.check_failed += 1,
            Status::Error => c.errored += 1,
        }
    }
    c
}

fn render(f: &mut Frame, v: &View) {
    let area = f.area();
    if area.width < 8 || area.height < 6 {
        return;
    }
    let c = tally(v.states);
    let show_recent = area.height >= RECENT_MIN_HEIGHT && !v.recent.is_empty();

    // Content flows top-down; the footer chrome pins to the bottom and a flex
    // spacer absorbs any slack between them (no void mid-column).
    let top = 5u16; // header(2) · rule · counts · spacer
    let bottom = 2u16; // rule · footer
    let recent_block = if show_recent {
        RECENT_SHOWN as u16 + 2
    } else {
        0
    }; // rule · label · rows
    let grid_budget = area
        .height
        .saturating_sub(top + bottom + recent_block)
        .max(1) as usize;
    let grid_lines = cap_lines(
        build_grid(v, area.width as usize, grid_budget),
        grid_budget,
        v.prefs,
    );
    let grid_h = grid_lines.len().max(1) as u16;

    let mut constraints = vec![
        Constraint::Length(2),      // header
        Constraint::Length(1),      // rule
        Constraint::Length(1),      // counts
        Constraint::Length(1),      // spacer
        Constraint::Length(grid_h), // grid (sized to content)
    ];
    if show_recent {
        constraints.push(Constraint::Length(1)); // rule
        constraints.push(Constraint::Length(RECENT_SHOWN as u16 + 1)); // recent
    }
    constraints.push(Constraint::Min(0)); // flex spacer → slack sits above the footer
    constraints.push(Constraint::Length(1)); // rule
    constraints.push(Constraint::Length(1)); // footer
    let z = Layout::vertical(constraints).split(area);

    let mut i = 0;
    let mut next = || {
        let r = z[i];
        i += 1;
        r
    };

    render_header(f, next(), v, c);
    render_rule(f, next(), v.prefs);
    render_counts(f, next(), v, c);
    let _spacer = next();
    f.render_widget(Paragraph::new(grid_lines), next());
    if show_recent {
        render_rule(f, next(), v.prefs);
        render_recent(f, next(), v);
    }
    let _flex = next();
    render_rule(f, next(), v.prefs);
    render_footer(f, next(), v, c);
}

fn render_rule(f: &mut Frame, area: Rect, prefs: Prefs) {
    let line = Line::from(Span::styled(
        style::RULE.repeat(area.width as usize),
        prefs.fg(style::BORDER).dim(),
    ));
    f.render_widget(Paragraph::new(line), area);
}

fn render_header(f: &mut Frame, area: Rect, v: &View, c: Counts) {
    let p = v.prefs;
    let width = area.width as usize;

    // Right rail: "<done>/<total>" + 10-dot LED progress meter.
    let frac = format!("{}/{} ", c.done(), c.total);
    let mut right: Vec<Span> = vec![Span::styled(frac, p.fg(style::MUTED))];
    right.extend(meter_spans(c.done(), c.total, p));

    // Left rail: wordmark + log path, truncated to leave room for the meter.
    let right_w: usize = span_width(&right);
    let head = "iris-bench";
    let sep = format!("  {} ", style::SEP);
    let fixed = head.chars().count() + sep.chars().count() + right_w + 2;
    let log = v.spec.log_path.display().to_string();
    let log = style::truncate_left(&log, width.saturating_sub(fixed).max(4));

    let mut left: Vec<Span> = vec![
        Span::styled(head, p.fg(style::ACCENT).bold()),
        Span::styled(sep, p.fg(style::MUTED)),
        Span::styled(log, p.fg(style::MUTED)),
    ];
    let pad = width.saturating_sub(span_width(&left) + right_w).max(1);
    left.push(Span::raw(" ".repeat(pad)));
    left.extend(right);

    // Config metadata line.
    let arms: Vec<&str> = v.spec.arms.iter().map(|a| a.label()).collect();
    let workloads = v.spec.workloads.len();
    let meta = format!(
        "models {} {sep} reasoning {} {sep} {} workload{} {sep} {} {sep} N {} {sep} conc {}",
        style::truncate(&v.spec.models.join(","), 34),
        v.spec.reasoning.as_deref().unwrap_or("none"),
        workloads,
        if workloads == 1 { "" } else { "s" },
        arms.join("/"),
        v.spec.runs,
        v.spec.effective_concurrency(),
        sep = style::SEP,
    );

    let lines = vec![
        Line::from(left),
        Line::from(Span::styled(
            style::truncate(&meta, width),
            p.fg(style::MUTED),
        )),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

/// The 10-dot LED progress meter: filled `●` (muted, accent at the fill edge),
/// empty `○` (dim). The Iris meter idiom, replacing an animated percentage bar.
fn meter_spans(done: usize, total: usize, p: Prefs) -> Vec<Span<'static>> {
    let filled = if total == 0 {
        0
    } else {
        ((done as f64 / total as f64) * METER_DOTS as f64).round() as usize
    }
    .min(METER_DOTS);
    (0..METER_DOTS)
        .map(|i| {
            if i < filled {
                let edge = i + 1 == filled;
                let color = if edge { style::ACCENT } else { style::MUTED };
                Span::styled(style::RUNNING, p.fg(color))
            } else {
                Span::styled(style::QUEUED, p.fg(style::MUTED).dim())
            }
        })
        .collect()
}

fn render_counts(f: &mut Frame, area: Rect, v: &View, c: Counts) {
    let p = v.prefs;
    let items = [
        (Status::Success, c.success),
        (Status::CheckFailed, c.check_failed),
        (Status::Error, c.errored),
        (Status::Running, c.running),
        (Status::Pending, c.pending),
    ];
    let mut spans: Vec<Span> = Vec::new();
    for (status, n) in items {
        let (glyph, color, label) = status.mark();
        spans.push(Span::styled(format!("{glyph} "), p.fg(color)));
        spans.push(Span::styled(format!("{n}"), p.fg(style::BORDER).bold()));
        spans.push(Span::styled(format!(" {label}    "), p.fg(style::MUTED)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Build the grid lines: the structured matrix when it fits, else the compact
/// wrapped fallback.
fn build_grid(v: &View, width: usize, height: usize) -> Vec<Line<'static>> {
    structured_grid(v, width, height).unwrap_or_else(|| flat_grid(v, width, height))
}

/// A glyph span for one cell, bold when live so active work reads at a glance.
fn cell_span(status: Status, p: Prefs) -> Span<'static> {
    let (glyph, color, _) = status.mark();
    let style = if matches!(status, Status::Running) {
        p.fg(color).bold()
    } else {
        p.fg(color)
    };
    Span::styled(glyph, style)
}

/// The matrix view: one block per model, one row per workload, arm groups
/// separated by a gap. Returns `None` when it would not fit the area (too tall
/// or too wide), so the caller falls back to the compact wrapped grid.
fn structured_grid(v: &View, width: usize, height: usize) -> Option<Vec<Line<'static>>> {
    let p = v.prefs;
    let models = &v.spec.models;
    let workloads = &v.spec.workloads;
    if models.is_empty() || workloads.is_empty() {
        return None;
    }

    // Bucket each cell's status by (model, workload, arm-label), preserving run
    // order (the expansion is run-consecutive within a cell).
    let mut buckets: HashMap<(&str, &str, &str), Vec<Status>> = HashMap::new();
    for (cell, st) in v.cells.iter().zip(v.states) {
        buckets
            .entry((
                cell.model.as_str(),
                cell.workload.as_str(),
                cell.arm.label(),
            ))
            .or_default()
            .push(st.status());
    }

    let label_w = workloads
        .iter()
        .map(|w| w.chars().count())
        .max()
        .unwrap_or(0)
        .min(WL_LABEL_MAX);
    let arms: Vec<&str> = v.spec.arms.iter().map(|a| a.label()).collect();
    // 2 indent + label + 2 gap + arms*runs glyphs + gaps between arms.
    let row_w = 2 + label_w + 2 + arms.len() * v.spec.runs + arms.len().saturating_sub(1) * 2;
    let need_h =
        models.iter().map(|_| 1 + workloads.len()).sum::<usize>() + models.len().saturating_sub(1);
    if row_w > width || need_h > height {
        return None;
    }

    let mut lines: Vec<Line> = Vec::with_capacity(need_h);
    for (mi, model) in models.iter().enumerate() {
        if mi > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            style::truncate(model, width),
            p.fg(style::INTERACTIVE).bold(),
        )));
        for wl in workloads {
            let mut spans: Vec<Span> = vec![Span::raw("  ")];
            let padded = format!("{:<label_w$}", style::truncate(wl, label_w));
            spans.push(Span::styled(padded, p.fg(style::MUTED)));
            spans.push(Span::raw("  "));
            for (ai, arm) in arms.iter().enumerate() {
                if ai > 0 {
                    spans.push(Span::raw("  "));
                }
                match buckets.get(&(model.as_str(), wl.as_str(), *arm)) {
                    Some(runs) => {
                        for st in runs {
                            spans.push(cell_span(*st, p));
                        }
                    }
                    None => spans.push(Span::styled(
                        style::QUEUED.repeat(v.spec.runs),
                        p.fg(style::MUTED).dim(),
                    )),
                }
            }
            lines.push(Line::from(spans));
        }
    }
    Some(lines)
}

/// Compact fallback: each model's cells wrapped to width under a dim label
/// (label omitted for a single-model run). Always fits by construction.
fn flat_grid(v: &View, width: usize, _height: usize) -> Vec<Line<'static>> {
    let p = v.prefs;
    let cols = width.saturating_sub(2).max(1);
    let multi = v.spec.models.len() > 1;
    let mut lines: Vec<Line> = Vec::new();

    for (mi, model) in v.spec.models.iter().enumerate() {
        if multi {
            if mi > 0 {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                style::truncate(model, width),
                p.fg(style::INTERACTIVE).bold(),
            )));
        }
        let mut spans: Vec<Span> = vec![Span::raw("  ")];
        let mut in_row = 0usize;
        for (cell, st) in v.cells.iter().zip(v.states) {
            if &cell.model != model {
                continue;
            }
            if in_row == cols {
                lines.push(Line::from(std::mem::take(&mut spans)));
                spans.push(Span::raw("  "));
                in_row = 0;
            }
            spans.push(cell_span(st.status(), p));
            in_row += 1;
        }
        if in_row > 0 {
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// Clip `lines` to `height`, replacing the last visible row with a dim overflow
/// note when something was dropped (never silently truncate coverage).
fn cap_lines(mut lines: Vec<Line<'static>>, height: usize, p: Prefs) -> Vec<Line<'static>> {
    if lines.len() <= height {
        return lines;
    }
    let hidden = lines.len() - height + 1;
    lines.truncate(height.saturating_sub(1));
    lines.push(Line::from(Span::styled(
        format!("{} +{hidden} more rows", style::ELLIPSIS),
        p.fg(style::MUTED).dim(),
    )));
    lines
}

fn render_recent(f: &mut Frame, area: Rect, v: &View) {
    let p = v.prefs;
    let width = area.width as usize;
    let mut lines: Vec<Line> = vec![Line::from(Span::styled("recent", p.fg(style::MUTED).dim()))];
    for r in v.recent.iter().take(RECENT_SHOWN) {
        let (glyph, color, _) = r.status.mark();
        let sep = format!(" {} ", style::SEP);
        let mut spans: Vec<Span> = vec![
            Span::styled(format!("  {glyph} "), p.fg(color)),
            Span::styled(style::truncate(&r.model, 22), p.fg(style::BORDER)),
            Span::styled(sep.clone(), p.fg(style::MUTED)),
            Span::styled(style::truncate(&r.workload, 20), p.fg(style::MUTED)),
            Span::styled(format!("{sep}{} #{}", r.arm, r.run), p.fg(style::MUTED)),
        ];
        if r.status == Status::Error {
            spans.push(Span::styled(sep, p.fg(style::MUTED)));
            spans.push(Span::styled(
                style::truncate(&r.note, 40),
                p.fg(style::DANGER),
            ));
        } else {
            spans.push(Span::styled(
                format!(
                    "{sep}{} turns{sep}{}{}",
                    r.turns,
                    style::IN_TOK,
                    style::humanize_tokens(r.input_tokens),
                ),
                p.fg(style::MUTED),
            ));
        }
        lines.push(Line::from(clip_spans(spans, width)));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn render_footer(f: &mut Frame, area: Rect, v: &View, c: Counts) {
    let p = v.prefs;
    let sep = format!(" {} ", style::SEP);
    let elapsed = fmt_elapsed(v.elapsed);

    let spans: Vec<Span> = if v.cancelling && !c.all_settled() {
        vec![
            Span::styled(format!("{} ", style::ERROR), p.fg(style::DANGER)),
            Span::styled(
                format!("cancelling{sep}draining in-flight cells{}", style::ELLIPSIS),
                p.fg(style::MUTED),
            ),
        ]
    } else if c.all_settled() {
        let mut s = vec![
            Span::styled(format!("{} ", style::DONE), p.fg(style::SUCCESS)),
            Span::styled(
                format!(
                    "done{sep}{} ok · {} check-fail · {} err{sep}{elapsed}",
                    c.success, c.check_failed, c.errored,
                ),
                p.fg(style::MUTED),
            ),
        ];
        s.extend(delta_spans(v, &sep, true));
        s
    } else {
        let pos = if p.reduced_motion {
            style::CHASE_LEN // pin the head (static readout)
        } else {
            style::chase_pos(v.elapsed.as_millis(), CHASE_STEP_MS)
        };
        let mut s: Vec<Span> = style::chase_cells(pos)
            .into_iter()
            .map(|cell| {
                let color = if cell == style::RUNNING {
                    style::ACCENT
                } else {
                    style::MUTED
                };
                Span::styled(cell, p.fg(color))
            })
            .collect();
        s.push(Span::styled(format!("  {elapsed}"), p.fg(style::MUTED)));
        s.extend(delta_spans(v, &sep, false));
        s.push(Span::styled(
            format!("{sep}ESC to cancel"),
            p.fg(style::MUTED).dim(),
        ));
        s
    };
    f.render_widget(
        Paragraph::new(Line::from(clip_spans(spans, area.width as usize))),
        area,
    );
}

/// The live paired token delta: `defaults −18.4% input · 3 pairs`, colored by
/// direction (green = savings, red = regression) and paired with a signed
/// number so it stays honest and monochrome-legible. Silent until a real pair
/// exists.
fn delta_spans(v: &View, sep: &str, settled: bool) -> Vec<Span<'static>> {
    let p = v.prefs;
    match v.delta {
        Some((pct, pairs)) => {
            let color = if pct < -0.05 {
                style::SUCCESS
            } else if pct > 0.05 {
                style::DANGER
            } else {
                style::MUTED
            };
            vec![
                Span::styled(format!("{sep}defaults "), p.fg(style::MUTED)),
                Span::styled(style::signed_pct(pct), p.fg(color)),
                Span::styled(
                    format!(
                        " input {} {pairs} pair{}",
                        style::SEP,
                        if pairs == 1 { "" } else { "s" }
                    ),
                    p.fg(style::MUTED),
                ),
            ]
        }
        // Before any pair completes, only nudge during the run (the settled line
        // never claims a measurement it doesn't have).
        None if !settled => vec![Span::styled(
            format!("{sep}pairing{}", style::ELLIPSIS),
            p.fg(style::MUTED).dim(),
        )],
        None => Vec::new(),
    }
}

// ── small helpers ─────────────────────────────────────────────────────────

fn span_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Drop whole trailing spans that would overflow `width` (keeps styling intact;
/// never splits mid-span). A partial last span is truncated to the remainder.
fn clip_spans<'a>(spans: Vec<Span<'a>>, width: usize) -> Vec<Span<'a>> {
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    for s in spans {
        let w = s.content.chars().count();
        if used + w <= width {
            used += w;
            out.push(s);
        } else {
            let room = width.saturating_sub(used);
            if room > 0 {
                let text: String = s.content.chars().take(room).collect();
                out.push(Span::styled(text, s.style));
            }
            break;
        }
    }
    out
}

fn fmt_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_agent::harness::Arm;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn spec(models: &[&str], workloads: &[&str], runs: usize) -> RunSpec {
        RunSpec {
            models: models.iter().map(|s| s.to_string()).collect(),
            reasoning: Some("low".into()),
            workloads: workloads.iter().map(|s| s.to_string()).collect(),
            arms: vec![Arm::Baseline, Arm::Defaults],
            runs,
            concurrency: 4,
            log_path: PathBuf::from("target/iris-bench-runs.jsonl"),
            allow_skip_permissions: false,
        }
    }

    fn record(
        model: &str,
        workload: &str,
        arm: Arm,
        run: usize,
        input: u64,
        ok: bool,
    ) -> CellRecord {
        let mut r = CellRecord::error(model, workload, arm.label(), run, "x");
        r.kind = "real_cell".into();
        r.valid = true;
        r.success = ok;
        r.input_tokens = input;
        r.turns = 5;
        r.error = None;
        r
    }

    fn buffer_text(states: &[CellState], v: &View) -> String {
        let mut term = Terminal::new(TestBackend::new(96, 30)).unwrap();
        let _ = states;
        term.draw(|f| render(f, v)).unwrap();
        let buf = term.backend().buffer().clone();
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    fn view<'a>(
        spec: &'a RunSpec,
        cells: &'a [Cell],
        states: &'a [CellState],
        recent: &'a VecDeque<Recent>,
        delta: Option<(f64, usize)>,
        cancelling: bool,
    ) -> View<'a> {
        View {
            spec,
            cells,
            states,
            recent,
            delta,
            elapsed: Duration::from_secs(84),
            cancelling,
            prefs: Prefs::default(),
        }
    }

    #[test]
    fn footer_settles_when_all_cells_finish() {
        let s = spec(&["m"], &["w"], 2);
        let cells = s.expand();
        let done: Vec<CellState> = cells
            .iter()
            .map(|_| {
                let mut cs = CellState::default();
                cs.set(Status::Success);
                cs
            })
            .collect();
        let recent = VecDeque::new();
        // Settled run: the footer reports "done", never the chase or "pairing…".
        let v = view(&s, &cells, &done, &recent, Some((-12.0, 1)), false);
        let text = buffer_text(&done, &v);
        assert!(text.contains(&format!("{} done", style::DONE)));
        assert!(!text.contains("ESC to cancel"));
        assert!(!text.contains("pairing"));
    }

    #[test]
    fn delta_only_counts_complete_pairs() {
        let mut d = Delta::default();
        d.observe(&record("m", "w", Arm::Baseline, 1, 1000, true));
        assert_eq!(d.summary(), None, "one arm is not a pair");
        d.observe(&record("m", "w", Arm::Defaults, 1, 800, true));
        let (pct, pairs) = d.summary().unwrap();
        assert_eq!(pairs, 1);
        assert!((pct - (-20.0)).abs() < 1e-9, "800 vs 1000 = -20%");
        // Invalid (error) rows never move the number.
        let mut err = record("m", "w2", Arm::Baseline, 1, 500, false);
        err.valid = false;
        d.observe(&err);
        assert_eq!(d.summary().unwrap().1, 1);
    }

    #[test]
    fn renders_full_frame_with_glyphs_no_panic() {
        let s = spec(&["anthropic:claude-haiku-4-5"], &["fix_test", "rename"], 3);
        let cells = s.expand();
        let mut states = vec![CellState::default(); cells.len()];
        states[0].set(Status::Success);
        states[1].set(Status::CheckFailed);
        states[2].set(Status::Running);
        states[3].set(Status::Error);
        let mut recent = VecDeque::new();
        push_recent(
            &mut recent,
            Recent {
                status: Status::Success,
                model: "anthropic:claude-haiku-4-5".into(),
                workload: "fix_test".into(),
                arm: "defaults".into(),
                run: 1,
                turns: 6,
                input_tokens: 14200,
                note: String::new(),
            },
        );
        let v = view(&s, &cells, &states, &recent, Some((-18.4, 3)), false);
        let text = buffer_text(&states, &v);
        // Symbol vocabulary present; ASCII stand-ins absent.
        assert!(text.contains(style::DONE));
        assert!(text.contains(style::WARN));
        assert!(text.contains(style::ERROR));
        assert!(text.contains(style::RUNNING));
        assert!(text.contains(style::SEP), "uses ┊ not |");
        assert!(text.contains(style::MINUS), "uses − in the delta");
        assert!(text.contains("14.2k"));
        assert!(!text.contains('|'), "no ASCII pipe separators");
        assert!(!text.contains("..."), "no ASCII ellipsis");
    }

    #[test]
    fn structured_grid_falls_back_when_too_tall() {
        // Many workloads in a short area cannot fit the per-workload matrix.
        let wls: Vec<String> = (0..40).map(|i| format!("workload_{i}")).collect();
        let wl_refs: Vec<&str> = wls.iter().map(String::as_str).collect();
        let s = spec(&["m"], &wl_refs, 2);
        let cells = s.expand();
        let states = vec![CellState::default(); cells.len()];
        let recent = VecDeque::new();
        let v = view(&s, &cells, &states, &recent, None, false);
        assert!(structured_grid(&v, 96, 10).is_none());
        // Flat fallback always produces something bounded.
        let flat = flat_grid(&v, 96, 10);
        assert!(!flat.is_empty());
    }

    #[test]
    fn monochrome_frame_still_carries_state() {
        let s = spec(&["m"], &["w"], 1);
        let cells = s.expand();
        let mut states = vec![CellState::default(); cells.len()];
        states[0].set(Status::Error);
        let recent = VecDeque::new();
        let mut v = view(&s, &cells, &states, &recent, None, false);
        v.prefs = Prefs {
            mono: true,
            reduced_motion: true,
        };
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| render(f, &v)).unwrap();
        let buf = term.backend().buffer().clone();
        // No cell carries a foreground color in monochrome mode.
        let colored = buf
            .content()
            .iter()
            .any(|c| c.fg != ratatui::style::Color::Reset);
        assert!(!colored, "NO_COLOR must render without foreground color");
        // Yet the error state is still present as a glyph + label.
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains(style::ERROR) && text.contains("err"));
    }
}
