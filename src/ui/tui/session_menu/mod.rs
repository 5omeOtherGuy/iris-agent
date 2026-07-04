//! SessionBar dropdowns (top chrome): the directory tree and the git console.
//!
//! One shared slot, two occupants. A [`SessionMenu`] renders BETWEEN the
//! session-bar row and its soft hairline (the hairline becomes the dropdown's
//! closing rule), pushing the transcript down. It is top chrome, not an
//! overlay: plain `bg` fill, no shadow, no scrim, no box frame. At most one
//! dropdown is open; opening one closes the other, and a docked modal or
//! approval closes the dropdown ([`super::Screen::open_modal`]).
//!
//! List-state law: while a dropdown LIST has focus there is no free typing --
//! single-letter commands are legal only there. Any INPUT row (filter, create)
//! makes printable keys text, always. While a turn runs dropdowns open as
//! READOUTS: rows dim, every mutating key is a no-op, and the footer says so.
//!
//! Both menus are pure state + render (the `startup.rs` Component idiom); side
//! effects travel as [`MenuAction`]s the loop executes at the idle boundary,
//! wired to the existing `GitSafety` settlement API -- never a re-implemented
//! rollback/accept.

mod git_menu;
mod tree_menu;

use std::path::PathBuf;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::git::status::GitStatus;
use crate::ui::palette;

pub(crate) use git_menu::GitMenu;
pub(crate) use tree_menu::TreeMenu;

#[cfg(test)]
use super::wrap::line_text;
use super::wrap::{display_width, truncate_line};
use super::{dim_style, prompt_style};

/// Height cap for a dropdown: at most this many rows, or ⅓ of the pane height,
/// whichever is smaller.
pub(crate) const MAX_DROPDOWN_ROWS: usize = 16;

/// A neutral key for the dropdown state machines (mapped from crossterm in
/// `tui_loop.rs`, mirroring `ModalKey`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuKey {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Tab,
    Backspace,
    /// Readline delete-word inside an input row.
    CtrlW,
    Char(char),
}

/// A side effect the loop must perform (idle boundary only). The menu never
/// touches git or task state itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MenuAction {
    /// `GitSafety::accept()` the unsettled task, then close with its summary.
    Accept,
    /// Accept the task, then check out `branch` (the settle-first switch path).
    AcceptThenCheckout { branch: String },
    /// Fetch `restore_points()` and hand them back via
    /// [`GitMenu::set_restore_points`].
    LoadRestorePoints,
    /// `GitSafety::rollback(seq)`; surface summary/preserved/index notices.
    Rollback { seq: u64 },
    /// Plain `git checkout <branch>` (carry is protected by the ledger).
    Checkout { branch: String },
    /// `git stash push` then checkout (the dirty-switch stash path).
    StashCheckout { branch: String },
    /// `git checkout -b <name> <base>`.
    CreateBranch { name: String, base: String },
    /// `git worktree add <path> -b <name> <base>`; on success the menu shows
    /// the in-dropdown "worktree ready" confirm.
    CreateWorktree {
        name: String,
        base: String,
        path: PathBuf,
    },
    /// Re-anchor the session in another worktree (idle-only).
    OpenSessionAt {
        path: PathBuf,
        branch: Option<String>,
    },
    /// Insert `@<path> ` into the composer at the cursor and close.
    InsertReference(String),
}

/// What a routed key did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MenuOutcome {
    /// Not consumed / no visible change.
    Ignore,
    /// State changed; redraw.
    Redraw,
    /// Close the dropdown (esc / activation completed).
    Close,
    /// Close-independent side effect for the loop to execute.
    Action(MenuAction),
}

/// The one SessionBar dropdown slot.
pub(crate) enum SessionMenu {
    Tree(TreeMenu),
    Git(GitMenu),
}

impl SessionMenu {
    /// Render the dropdown's rows (including its internal rule + footer hints)
    /// for `width` columns, capped to `max_rows`. `readonly` = a turn is
    /// running (readout mode).
    pub(crate) fn render_lines(
        &self,
        width: usize,
        max_rows: usize,
        readonly: bool,
        git: Option<&GitStatus>,
        referenced: &[String],
    ) -> Vec<Line<'static>> {
        match self {
            SessionMenu::Git(menu) => menu.render_lines(width, max_rows, readonly),
            SessionMenu::Tree(menu) => {
                menu.render_lines(width, max_rows, readonly, git, referenced)
            }
        }
    }

    /// Route one key. `readonly` = a turn is running: every mutating key is a
    /// no-op, navigation and `esc` stay live.
    pub(crate) fn handle_key(&mut self, key: MenuKey, readonly: bool) -> MenuOutcome {
        match self {
            SessionMenu::Git(menu) => menu.handle_key(key, readonly),
            SessionMenu::Tree(menu) => menu.handle_key(key, readonly),
        }
    }

    /// Mouse click on a dropdown line (0-based below the session-bar row):
    /// first click selects, second activates.
    pub(crate) fn click_line(&mut self, line: usize, readonly: bool) -> MenuOutcome {
        match self {
            SessionMenu::Git(menu) => menu.click_line(line, readonly),
            SessionMenu::Tree(menu) => menu.click_line(line, readonly),
        }
    }
}

// --- shared render vocabulary -------------------------------------------

/// An UPPERCASE group label: dim bold (label tracking is a web concern; the
/// terminal carries it with case + weight alone).
pub(super) fn group_label(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        dim_style().add_modifier(Modifier::BOLD),
    ))
}

/// One selectable dropdown row in the house picker idiom: a 1-col `◉` marker
/// (orange when marked), the label, a dim dotted leader, and a right-aligned
/// dim meta. The selected row gets the `surface` fill across the row width.
pub(super) fn menu_row(
    marked: bool,
    label: Vec<Span<'static>>,
    meta: Vec<Span<'static>>,
    selected: bool,
    width: usize,
) -> Line<'static> {
    let marker = if marked {
        Span::styled(format!("{} ", crate::ui::symbols::ACTIVE), prompt_style())
    } else {
        Span::raw("  ".to_string())
    };
    let mut label = label;
    if selected {
        for span in &mut label {
            span.style = span.style.add_modifier(Modifier::BOLD);
        }
    }
    let label_w: usize = label.iter().map(|s| display_width(&s.content)).sum();
    let meta_w: usize = meta.iter().map(|s| display_width(&s.content)).sum();
    // marker(2) + label + space + leader + space + meta, leader fills.
    let leader = width
        .saturating_sub(2)
        .saturating_sub(label_w)
        .saturating_sub(meta_w)
        .saturating_sub(2);
    let mut spans = vec![marker];
    spans.extend(label);
    if meta_w > 0 || leader > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled("·".repeat(leader), dim_style()));
        spans.push(Span::raw(" "));
    }
    spans.extend(meta);
    if selected {
        let fill = Style::default().bg(palette::SURFACE);
        for span in &mut spans {
            span.style = span.style.patch(fill);
        }
    }
    let mut line = Line::from(spans);
    truncate_line(&mut line, width.max(1));
    line
}

/// The dim internal rule above the footer hints (`╌` repeat, lighter than the
/// session bar's closing hairline).
pub(super) fn internal_rule(width: usize) -> Line<'static> {
    Line::from(Span::styled("╌".repeat(width.max(1)), dim_style()))
}

/// A `┊`-separated footer of `key label` hints (keys in ink, labels muted).
pub(super) fn footer_hints(items: &[(&str, &str)], width: usize) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, label)) in items.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                format!(" {} ", crate::ui::symbols::SEP),
                dim_style(),
            ));
        }
        if key.is_empty() {
            spans.push(Span::styled((*label).to_string(), dim_style()));
        } else {
            spans.push(Span::raw((*key).to_string()));
            if !label.is_empty() {
                spans.push(Span::styled(format!(" {label}"), dim_style()));
            }
        }
    }
    let mut line = Line::from(spans);
    truncate_line(&mut line, width.max(1));
    line
}

/// The read-only footer shown while a turn runs.
pub(super) fn readonly_footer(width: usize) -> Line<'static> {
    let mut line = Line::from(vec![
        Span::styled(
            format!("{} agent running", crate::ui::symbols::RUNNING),
            prompt_style(),
        ),
        Span::styled(
            format!(
                " {} read-only — actions return when idle {} esc",
                crate::ui::symbols::SEP,
                crate::ui::symbols::SEP
            ),
            dim_style(),
        ),
    ]);
    truncate_line(&mut line, width.max(1));
    line
}

/// A filter match count with a correctly pluralized noun (`1 match`,
/// `3 matches`).
pub(super) fn match_count(count: usize) -> String {
    if count == 1 {
        "1 match".to_string()
    } else {
        format!("{count} matches")
    }
}

/// Apply the readout dimming to already-rendered rows (readonly mode, and the
/// dimmed list behind a confirm/create footer).
pub(super) fn dim_lines(lines: &mut [Line<'static>]) {
    for line in lines {
        for span in &mut line.spans {
            span.style = dim_style();
        }
    }
}

/// An input row: SURFACE fill, orange `▋` caret, typed text ink (danger role
/// when `invalid`), right-aligned dim hint spans.
pub(super) fn input_row(
    text: &str,
    invalid: bool,
    hint: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let text_style = if invalid {
        super::err_style()
    } else {
        Style::default()
    };
    let mut spans = vec![
        Span::raw(" "),
        Span::styled("▋".to_string(), prompt_style()),
        Span::styled(text.to_string(), text_style),
    ];
    let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
    let hint_w: usize = hint.iter().map(|s| display_width(&s.content)).sum();
    let pad = width.saturating_sub(used).saturating_sub(hint_w);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.extend(hint);
    let fill = Style::default().bg(palette::SURFACE);
    for span in &mut spans {
        span.style = span.style.patch(fill);
    }
    let mut line = Line::from(spans);
    truncate_line(&mut line, width.max(1));
    line
}

/// Case-insensitive (Unicode) subsequence fuzzy match (the filter idiom). These
/// dropdowns hold no lowercased haystack cache, so they lower both sides inline
/// with `str::to_lowercase` (full Unicode case folding) and defer the
/// subsequence check to the shared [`crate::ui::selector::fuzzy_match`].
pub(super) fn fuzzy_match(needle: &str, haystack: &str) -> bool {
    crate::ui::selector::fuzzy_match(&needle.to_lowercase(), &haystack.to_lowercase())
}

/// Wrap-around list step: advance `selected` by `delta` within a list of `len`
/// rows, wrapping past either end. `len == 0` yields 0. The single wrap-around
/// selection helper for the SessionBar dropdowns (git console + directory tree)
/// and their filter/rollback sublists -- replaces the per-menu `rem_euclid`
/// copies so no wrap math lives outside the shared primitives.
///
/// Wrap policy: these SPATIAL menus WRAP (pi-mono model-list feel), matching
/// [`crate::ui::selector::Selector`]'s `wrap = true`. The type-ahead slash
/// palette CLAMPS instead ([`crate::ui::slash::Palette`]) so a fast typist
/// never leaps the far end mid-filter. Two behaviors, one documented split.
pub(super) fn step_wrapped(selected: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let current = selected.min(len - 1) as isize;
    (current + delta).rem_euclid(len as isize) as usize
}

/// Home-relativize a path for display (`/home/u/x` → `~/x`).
pub(super) fn home_rel(path: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
        && let Ok(rel) = path.strip_prefix(std::path::Path::new(&home))
    {
        if rel.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
}

/// Truncate every rendered line to `width` and the block to `max_rows`,
/// keeping the last two rows (rule + footer) pinned when truncation bites.
pub(super) fn cap_block(mut lines: Vec<Line<'static>>, max_rows: usize) -> Vec<Line<'static>> {
    if lines.len() > max_rows && max_rows >= 3 {
        let tail: Vec<Line<'static>> = lines.split_off(lines.len() - 2);
        lines.truncate(max_rows - 2);
        lines.extend(tail);
    } else {
        lines.truncate(max_rows.max(1));
    }
    lines
}

#[cfg(test)]
pub(super) fn lines_text(lines: &[Line<'static>]) -> String {
    lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_match_is_subsequence_case_insensitive() {
        assert!(fuzzy_match("srs", "src/ui/screen.rs"));
        assert!(fuzzy_match("FEAT", "feat/split-statusline"));
        assert!(!fuzzy_match("zzz", "src/main.rs"));
        assert!(fuzzy_match("", "anything"));
        // Unicode case folding still holds after delegating to the shared fn.
        assert!(fuzzy_match("Ä", "strände"));
    }

    #[test]
    fn step_wrapped_wraps_past_both_ends() {
        // Forward past the end wraps to the top.
        assert_eq!(step_wrapped(2, 3, 1), 0);
        // Backward past the top wraps to the last row.
        assert_eq!(step_wrapped(0, 3, -1), 2);
        // Interior steps are plain.
        assert_eq!(step_wrapped(1, 3, 1), 2);
        assert_eq!(step_wrapped(1, 3, -1), 0);
        // Empty list is inert; an out-of-range cursor clamps before stepping.
        assert_eq!(step_wrapped(5, 0, 1), 0);
        assert_eq!(step_wrapped(9, 3, 1), 0);
    }

    #[test]
    fn menu_row_marks_leader_and_surface_fill() {
        let row = menu_row(
            true,
            vec![Span::raw("main".to_string())],
            vec![Span::styled("here".to_string(), dim_style())],
            true,
            40,
        );
        let text = line_text(&row);
        assert!(text.starts_with("◉ main"), "{text:?}");
        assert!(text.contains("···"), "{text:?}");
        assert!(text.trim_end().ends_with("here"), "{text:?}");
        assert!(
            row.spans
                .iter()
                .all(|span| span.style.bg == Some(palette::SURFACE)),
            "selected row carries the surface fill"
        );
        assert!(display_width(&text) <= 40);
    }

    #[test]
    fn cap_block_keeps_rule_and_footer_pinned() {
        let lines: Vec<Line<'static>> = (0..10).map(|i| Line::from(i.to_string())).collect();
        let capped = cap_block(lines, 5);
        assert_eq!(capped.len(), 5);
        assert_eq!(line_text(&capped[3]), "8");
        assert_eq!(line_text(&capped[4]), "9");
    }
}
