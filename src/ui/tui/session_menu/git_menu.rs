//! The git console dropdown: status line, TASK settlement surface, SWITCH
//! list, WORKTREES board, and the confirm/create/filter state machines.
//!
//! This is the interactive face of the ADR-0028 recovery notice: after
//! "unsettled Iris changes from 3 hours ago — view / accept / roll back /
//! ignore", opening this dropdown IS "view". Settlement goes through the
//! existing `GitSafety` API only (`accept` / `restore_points` / `rollback`),
//! carried as [`MenuAction`]s the loop executes at the idle boundary.

use std::path::PathBuf;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::git::status::{GitStatus, compact_age, human_age};
use crate::ui::symbols;

use super::super::wrap::truncate_line;
use super::super::{dim_style, err_style, prompt_style};
use super::{
    MenuAction, MenuKey, MenuOutcome, cap_block, dim_lines, footer_hints, fuzzy_match, group_label,
    home_rel, input_row, internal_rule, match_count, menu_row, readonly_footer, step_wrapped,
};

/// Visible branch rows in the SWITCH group.
const SWITCH_CAP: usize = 8;
/// Visible worktree rows.
const WORKTREE_CAP: usize = 4;

/// One selectable row of the rest-state list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowRef {
    Branch(usize),
    Worktree(usize),
}

/// The confirm footer replacing the key hints (the list above dims).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Confirm {
    /// Switch with an unsettled task: settlement first.
    TaskUnsettled { branch: String },
    /// Switch with a dirty tree, no task: carry / stash two-step.
    Dirty { branch: String },
    /// The branch (or row) lives in another worktree: re-anchor implied.
    WorktreeRedirect {
        branch: Option<String>,
        path: PathBuf,
    },
    /// Unmerged paths: switching disabled entirely.
    Conflicts,
    /// Detached HEAD: `n` promoted, `↵` leaves HEAD.
    Detached { branch: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    List,
    Confirm(Confirm),
    /// The restore-point sublist (`r`).
    Rollback {
        points: Vec<(u64, String)>,
        selected: usize,
    },
    /// The `n`/`w` create input.
    Create {
        input: String,
        base: String,
        worktree: bool,
    },
    /// The `/` filter input over the branch list.
    Filter {
        input: String,
        selected: usize,
    },
    /// Post-`w` confirm: `↵ open session there ┊ esc stay`.
    WorktreeReady {
        path: PathBuf,
    },
}

/// Git console state. Holds its own status snapshot (set at open, refreshed by
/// the loop when the cache lands a newer one) so render is pure.
pub(crate) struct GitMenu {
    status: GitStatus,
    /// Resolved `worktree_root` (config, default `../wt` off the main
    /// worktree root), for the create-worktree path preview.
    worktree_root: PathBuf,
    selected: usize,
    mode: Mode,
}

impl GitMenu {
    pub(crate) fn new(status: GitStatus, worktree_root: PathBuf) -> Self {
        Self {
            status,
            worktree_root,
            selected: 0,
            mode: Mode::List,
        }
    }

    /// Replace the snapshot when a fresh capture lands (paint last known).
    pub(crate) fn set_status(&mut self, status: GitStatus) {
        self.status = status;
        let rows = self.rows().len();
        if rows > 0 && self.selected >= rows {
            self.selected = rows - 1;
        }
    }

    /// Hand back the restore points fetched for [`MenuAction::LoadRestorePoints`].
    pub(crate) fn set_restore_points(&mut self, points: Vec<(u64, String)>) {
        self.mode = Mode::Rollback {
            points,
            selected: 0,
        };
    }

    /// Show the in-dropdown "worktree ready" confirm after a `w` create.
    pub(crate) fn worktree_ready(&mut self, path: PathBuf) {
        self.mode = Mode::WorktreeReady { path };
    }

    #[cfg(test)]
    pub(crate) fn input_active(&self) -> bool {
        matches!(self.mode, Mode::Create { .. } | Mode::Filter { .. })
    }

    // --- rows ---------------------------------------------------------

    /// Branches shown in the SWITCH group: current first, then recent order.
    fn switch_branches(&self) -> Vec<usize> {
        let mut order: Vec<usize> = Vec::new();
        let current = self.status.branch.as_deref();
        if let Some(index) = self
            .status
            .recent_branches
            .iter()
            .position(|b| Some(b.name.as_str()) == current)
        {
            order.push(index);
        }
        for (index, _) in self.status.recent_branches.iter().enumerate() {
            if !order.contains(&index) {
                order.push(index);
            }
        }
        order
    }

    /// The selectable rows in list order.
    fn rows(&self) -> Vec<RowRef> {
        let mut rows: Vec<RowRef> = self
            .switch_branches()
            .into_iter()
            .take(SWITCH_CAP)
            .map(RowRef::Branch)
            .collect();
        if self.status.worktrees.len() > 1 {
            for index in 0..self.status.worktrees.len().min(WORKTREE_CAP) {
                rows.push(RowRef::Worktree(index));
            }
        }
        rows
    }

    /// Branch names matching the active filter.
    fn filter_matches(&self, input: &str) -> Vec<usize> {
        self.switch_branches()
            .into_iter()
            .filter(|&i| fuzzy_match(input, &self.status.recent_branches[i].name))
            .collect()
    }

    // --- keys ---------------------------------------------------------

    pub(crate) fn handle_key(&mut self, key: MenuKey, readonly: bool) -> MenuOutcome {
        if readonly {
            // Readout mode: navigation + esc only; every mutating key no-ops.
            return match key {
                MenuKey::Esc => MenuOutcome::Close,
                MenuKey::Up => self.move_selection(-1),
                MenuKey::Down => self.move_selection(1),
                _ => MenuOutcome::Ignore,
            };
        }
        match std::mem::replace(&mut self.mode, Mode::List) {
            Mode::List => self.key_list(key),
            Mode::Confirm(confirm) => self.key_confirm(confirm, key),
            Mode::Rollback { points, selected } => self.key_rollback(points, selected, key),
            Mode::Create {
                input,
                base,
                worktree,
            } => self.key_create(input, base, worktree, key),
            Mode::Filter { input, selected } => self.key_filter(input, selected, key),
            Mode::WorktreeReady { path } => self.key_worktree_ready(path, key),
        }
    }

    fn move_selection(&mut self, delta: isize) -> MenuOutcome {
        let rows = self.rows().len();
        if rows == 0 {
            return MenuOutcome::Ignore;
        }
        self.selected = step_wrapped(self.selected, rows, delta);
        MenuOutcome::Redraw
    }

    /// The switch decision for `branch`: settlement first, then dirt, then
    /// worktree redirect, then detached, then a plain checkout.
    fn activate_branch(&mut self, index: usize) -> MenuOutcome {
        let info = &self.status.recent_branches[index];
        let name = info.name.clone();
        if Some(name.as_str()) == self.status.branch.as_deref() {
            return MenuOutcome::Close;
        }
        if self.status.unmerged > 0 {
            self.mode = Mode::Confirm(Confirm::Conflicts);
            return MenuOutcome::Redraw;
        }
        if let Some(path) = info.worktree.clone() {
            self.mode = Mode::Confirm(Confirm::WorktreeRedirect {
                branch: Some(name),
                path,
            });
            return MenuOutcome::Redraw;
        }
        if self.status.task.is_some() {
            self.mode = Mode::Confirm(Confirm::TaskUnsettled { branch: name });
            return MenuOutcome::Redraw;
        }
        if self.status.is_dirty() {
            self.mode = Mode::Confirm(Confirm::Dirty { branch: name });
            return MenuOutcome::Redraw;
        }
        if self.status.branch.is_none() {
            self.mode = Mode::Confirm(Confirm::Detached { branch: name });
            return MenuOutcome::Redraw;
        }
        MenuOutcome::Action(MenuAction::Checkout { branch: name })
    }

    fn activate_row(&mut self, row: RowRef) -> MenuOutcome {
        match row {
            RowRef::Branch(index) => self.activate_branch(index),
            RowRef::Worktree(index) => {
                let wt = &self.status.worktrees[index];
                if wt.is_current {
                    return MenuOutcome::Close;
                }
                self.mode = Mode::Confirm(Confirm::WorktreeRedirect {
                    branch: wt.branch.clone(),
                    path: wt.path.clone(),
                });
                MenuOutcome::Redraw
            }
        }
    }

    /// The create base: the selected row's branch at the moment of the
    /// keypress (default current; HEAD when detached).
    fn create_base(&self) -> String {
        let rows = self.rows();
        if let Some(RowRef::Branch(index)) = rows.get(self.selected)
            && let Some(info) = self.status.recent_branches.get(*index)
        {
            return info.name.clone();
        }
        self.status
            .branch
            .clone()
            .unwrap_or_else(|| "HEAD".to_string())
    }

    fn key_list(&mut self, key: MenuKey) -> MenuOutcome {
        match key {
            MenuKey::Esc => MenuOutcome::Close,
            MenuKey::Up => self.move_selection(-1),
            MenuKey::Down => self.move_selection(1),
            MenuKey::Enter => {
                let rows = self.rows();
                match rows.get(self.selected).cloned() {
                    Some(row) => self.activate_row(row),
                    None => MenuOutcome::Ignore,
                }
            }
            MenuKey::Char('a') if self.status.task.is_some() => {
                MenuOutcome::Action(MenuAction::Accept)
            }
            MenuKey::Char('r') if self.status.task.is_some() => {
                MenuOutcome::Action(MenuAction::LoadRestorePoints)
            }
            MenuKey::Char('n') => {
                self.mode = Mode::Create {
                    input: String::new(),
                    base: self.create_base(),
                    worktree: false,
                };
                MenuOutcome::Redraw
            }
            MenuKey::Char('w') => {
                self.mode = Mode::Create {
                    input: String::new(),
                    base: self.create_base(),
                    worktree: true,
                };
                MenuOutcome::Redraw
            }
            MenuKey::Char('/') => {
                self.mode = Mode::Filter {
                    input: String::new(),
                    selected: 0,
                };
                MenuOutcome::Redraw
            }
            _ => MenuOutcome::Ignore,
        }
    }

    fn key_confirm(&mut self, confirm: Confirm, key: MenuKey) -> MenuOutcome {
        if key == MenuKey::Esc {
            self.mode = Mode::List;
            return MenuOutcome::Redraw;
        }
        match (&confirm, key) {
            (Confirm::TaskUnsettled { branch }, MenuKey::Char('a')) => {
                MenuOutcome::Action(MenuAction::AcceptThenCheckout {
                    branch: branch.clone(),
                })
            }
            (Confirm::TaskUnsettled { .. }, MenuKey::Char('r')) => {
                MenuOutcome::Action(MenuAction::LoadRestorePoints)
            }
            (Confirm::TaskUnsettled { branch }, MenuKey::Enter) => {
                // Carry anyway: plain checkout; the ledger's divergence
                // machinery protects the unsettled paths.
                MenuOutcome::Action(MenuAction::Checkout {
                    branch: branch.clone(),
                })
            }
            (Confirm::Dirty { branch }, MenuKey::Enter) => {
                MenuOutcome::Action(MenuAction::Checkout {
                    branch: branch.clone(),
                })
            }
            (Confirm::Dirty { branch }, MenuKey::Char('s')) => {
                MenuOutcome::Action(MenuAction::StashCheckout {
                    branch: branch.clone(),
                })
            }
            (Confirm::WorktreeRedirect { branch, path }, MenuKey::Enter) => {
                MenuOutcome::Action(MenuAction::OpenSessionAt {
                    path: path.clone(),
                    branch: branch.clone(),
                })
            }
            (Confirm::Detached { branch }, MenuKey::Enter) => {
                MenuOutcome::Action(MenuAction::Checkout {
                    branch: branch.clone(),
                })
            }
            (Confirm::Detached { .. }, MenuKey::Char('n')) => {
                self.mode = Mode::Create {
                    input: String::new(),
                    base: "HEAD".to_string(),
                    worktree: false,
                };
                MenuOutcome::Redraw
            }
            _ => {
                self.mode = Mode::Confirm(confirm);
                MenuOutcome::Ignore
            }
        }
    }

    fn key_rollback(
        &mut self,
        points: Vec<(u64, String)>,
        selected: usize,
        key: MenuKey,
    ) -> MenuOutcome {
        match key {
            MenuKey::Esc => {
                self.mode = Mode::List;
                MenuOutcome::Redraw
            }
            MenuKey::Up | MenuKey::Down if !points.is_empty() => {
                let delta: isize = if key == MenuKey::Up { -1 } else { 1 };
                let next = step_wrapped(selected, points.len(), delta);
                self.mode = Mode::Rollback {
                    points,
                    selected: next,
                };
                MenuOutcome::Redraw
            }
            MenuKey::Enter if !points.is_empty() => {
                let seq = points[selected.min(points.len() - 1)].0;
                MenuOutcome::Action(MenuAction::Rollback { seq })
            }
            _ => {
                self.mode = Mode::Rollback { points, selected };
                MenuOutcome::Ignore
            }
        }
    }

    fn key_create(
        &mut self,
        mut input: String,
        base: String,
        worktree: bool,
        key: MenuKey,
    ) -> MenuOutcome {
        match key {
            MenuKey::Esc => {
                self.mode = Mode::List;
                return MenuOutcome::Redraw;
            }
            MenuKey::Tab => {
                self.mode = Mode::Create {
                    input,
                    base,
                    worktree: !worktree,
                };
                return MenuOutcome::Redraw;
            }
            MenuKey::Backspace => {
                input.pop();
            }
            MenuKey::CtrlW => {
                // Readline delete-word: trailing separators, then the word.
                while matches!(input.chars().last(), Some(c) if c == '/' || c.is_whitespace()) {
                    input.pop();
                }
                while matches!(input.chars().last(), Some(c) if c != '/' && !c.is_whitespace()) {
                    input.pop();
                }
            }
            MenuKey::Enter => {
                let valid = self.validate_name(&input).is_ok();
                if valid {
                    let name = input.clone();
                    if worktree {
                        let path = self.worktree_path(&name);
                        // Stay in create mode until the loop confirms; a
                        // success replaces it via `worktree_ready`.
                        self.mode = Mode::Create {
                            input,
                            base: base.clone(),
                            worktree,
                        };
                        return MenuOutcome::Action(MenuAction::CreateWorktree {
                            name,
                            base,
                            path,
                        });
                    }
                    return MenuOutcome::Action(MenuAction::CreateBranch { name, base });
                }
                self.mode = Mode::Create {
                    input,
                    base,
                    worktree,
                };
                return MenuOutcome::Ignore;
            }
            MenuKey::Char(c) if !c.is_control() => {
                input.push(c);
            }
            _ => {}
        }
        self.mode = Mode::Create {
            input,
            base,
            worktree,
        };
        MenuOutcome::Redraw
    }

    fn key_filter(&mut self, mut input: String, mut selected: usize, key: MenuKey) -> MenuOutcome {
        match key {
            MenuKey::Esc => {
                self.mode = Mode::List;
                return MenuOutcome::Redraw;
            }
            MenuKey::Up | MenuKey::Down => {
                let matches = self.filter_matches(&input);
                if !matches.is_empty() {
                    let delta: isize = if key == MenuKey::Up { -1 } else { 1 };
                    selected = step_wrapped(selected, matches.len(), delta);
                }
            }
            MenuKey::Enter => {
                let matches = self.filter_matches(&input);
                if let Some(&index) = matches.get(selected.min(matches.len().saturating_sub(1))) {
                    return self.activate_branch(index);
                }
                self.mode = Mode::Filter { input, selected };
                return MenuOutcome::Ignore;
            }
            MenuKey::Backspace => {
                input.pop();
                selected = 0;
            }
            MenuKey::CtrlW => {
                input.clear();
                selected = 0;
            }
            MenuKey::Char(c) if !c.is_control() => {
                input.push(c);
                selected = 0;
            }
            _ => {
                self.mode = Mode::Filter { input, selected };
                return MenuOutcome::Ignore;
            }
        }
        self.mode = Mode::Filter { input, selected };
        MenuOutcome::Redraw
    }

    fn key_worktree_ready(&mut self, path: PathBuf, key: MenuKey) -> MenuOutcome {
        match key {
            MenuKey::Enter => MenuOutcome::Action(MenuAction::OpenSessionAt { path, branch: None }),
            // "esc stay": dismiss the ready confirmation but stay in the
            // console (return to the list), mirroring the confirm dialogs'
            // `esc cancel`. A full close is `esc` again from the list.
            MenuKey::Esc => {
                self.mode = Mode::List;
                MenuOutcome::Redraw
            }
            _ => {
                self.mode = Mode::WorktreeReady { path };
                MenuOutcome::Ignore
            }
        }
    }

    /// Map a rendered line index (0-based below the session bar) to the
    /// selectable row it shows, mirroring [`Self::render_list`]'s layout.
    fn line_to_row(&self, line: usize) -> Option<usize> {
        let mut at = 1; // status line
        if self.status.task.is_some() {
            at += 1; // TASK label
            if self.status.iris_unsettled > 0 {
                at += 1;
            }
            if self.status.user_dirty > 0 {
                at += 1;
            }
        }
        let branches = self.switch_branches();
        let shown = branches.len().min(SWITCH_CAP);
        if shown > 0 {
            at += 1; // SWITCH label
        }
        if line >= at && line < at + shown {
            return Some(line - at);
        }
        at += shown;
        if branches.len() > SWITCH_CAP {
            at += 1; // overflow row
        }
        if self.status.worktrees.len() > 1 {
            at += 1; // WORKTREES label
            let wt_shown = self.status.worktrees.len().min(WORKTREE_CAP);
            if line >= at && line < at + wt_shown {
                return Some(shown + (line - at));
            }
        }
        None
    }

    /// Mouse click on a dropdown line: first click selects, second activates.
    pub(crate) fn click_line(&mut self, line: usize, readonly: bool) -> MenuOutcome {
        if !matches!(self.mode, Mode::List) {
            return MenuOutcome::Ignore;
        }
        let Some(row) = self.line_to_row(line) else {
            return MenuOutcome::Ignore;
        };
        if self.selected == row {
            if readonly {
                return MenuOutcome::Ignore;
            }
            let rows = self.rows();
            return match rows.get(row).cloned() {
                Some(row) => self.activate_row(row),
                None => MenuOutcome::Ignore,
            };
        }
        self.selected = row;
        MenuOutcome::Redraw
    }

    // --- validation -----------------------------------------------------

    /// Local `git check-ref-format --branch` approximation plus a collision
    /// check against the known branch list. `Err` = invalid (gates `↵`).
    fn validate_name(&self, name: &str) -> Result<(), ()> {
        if !valid_branch_name(name) {
            return Err(());
        }
        if self
            .status
            .recent_branches
            .iter()
            .any(|branch| branch.name == name)
        {
            return Err(());
        }
        Ok(())
    }

    /// Worktree path preview: `<worktree_root>/<name with / → −>`.
    fn worktree_path(&self, name: &str) -> PathBuf {
        self.worktree_root.join(name.replace('/', "-"))
    }

    // --- render ---------------------------------------------------------

    pub(crate) fn render_lines(
        &self,
        width: usize,
        max_rows: usize,
        readonly: bool,
    ) -> Vec<Line<'static>> {
        let mut lines = match &self.mode {
            Mode::Rollback { points, selected } => self.render_rollback(width, points, *selected),
            _ => self.render_list(width, readonly),
        };
        for line in &mut lines {
            truncate_line(line, width.max(1));
        }
        cap_block(lines, max_rows)
    }

    /// The dim one-line status summary; zero-valued segments omitted.
    fn status_line(&self, width: usize) -> Line<'static> {
        let status = &self.status;
        let sep = format!(" {} ", symbols::SEP);
        let mut parts: Vec<String> = Vec::new();
        match (&status.branch, &status.detached_at) {
            (Some(branch), _) => match &status.upstream {
                Some(upstream) => parts.push(format!("{branch} → {upstream}")),
                None => parts.push(format!("{branch} → no upstream")),
            },
            (None, Some(at)) => {
                let (sha, subject) = at.split_once(' ').unwrap_or((at.as_str(), ""));
                if subject.is_empty() {
                    parts.push(format!("{} detached at {sha}", symbols::ERROR));
                } else {
                    parts.push(format!(
                        "{} detached at {sha} · \"{subject}\"",
                        symbols::ERROR
                    ));
                }
            }
            (None, None) => parts.push(format!("{} detached", symbols::ERROR)),
        }
        if status.is_dirty() {
            let mut dirt: Vec<String> = Vec::new();
            if status.task.is_some() {
                if status.user_dirty > 0 {
                    dirt.push(format!("{}{} yours", symbols::DIRTY, status.user_dirty));
                }
            } else if status.modified > 0 {
                dirt.push(format!("{}{} modified", symbols::DIRTY, status.modified));
            }
            if status.staged > 0 {
                dirt.push(format!("{} staged", status.staged));
            }
            if status.untracked > 0 {
                dirt.push(format!("{} untracked", status.untracked));
            }
            if status.unmerged > 0 {
                dirt.push(format!("{} conflicts", status.unmerged));
            }
            if !dirt.is_empty() {
                parts.push(dirt.join(" · "));
            }
        } else {
            parts.push("clean".to_string());
        }
        let mut sync = String::new();
        if status.ahead > 0 {
            sync.push_str(&format!("{}{}", symbols::AHEAD, status.ahead));
        }
        if status.behind > 0 {
            if !sync.is_empty() {
                sync.push(' ');
            }
            sync.push_str(&format!("{}{}", symbols::BEHIND, status.behind));
        }
        if !sync.is_empty() {
            parts.push(sync);
        }
        if status.stash > 0 {
            parts.push(format!("stash {}", status.stash));
        }
        if let Some(age) = status.last_commit_age {
            parts.push(human_age(age));
        }
        let mut line = Line::from(Span::styled(parts.join(&sep), dim_style()));
        truncate_line(&mut line, width.max(1));
        line
    }

    /// Truncating `a.rs · b.rs · +N more` file hint for the TASK rows.
    fn file_hint(paths: &[String], cap: usize) -> String {
        let names: Vec<&str> = paths
            .iter()
            .map(|p| p.rsplit('/').next().unwrap_or(p.as_str()))
            .collect();
        if names.is_empty() {
            return String::new();
        }
        let shown = names.iter().take(cap).copied().collect::<Vec<_>>();
        let mut hint = shown.join(" · ");
        if names.len() > cap {
            hint.push_str(&format!(" · +{} more", names.len() - cap));
        }
        hint
    }

    fn render_list(&self, width: usize, readonly: bool) -> Vec<Line<'static>> {
        let status = &self.status;
        let mut lines = vec![self.status_line(width)];
        let in_footer_state = !matches!(self.mode, Mode::List);
        let selecting = matches!(self.mode, Mode::List) && !readonly;

        // TASK group — the settlement surface, present only while unsettled.
        if let Some(task) = &status.task {
            let short: String = task.task_id.chars().take(8).collect();
            lines.push(group_label(&format!(
                "TASK — unsettled · {short} · {}",
                human_age(task.age)
            )));
            if status.iris_unsettled > 0 {
                lines.push(menu_row(
                    false,
                    vec![Span::styled(
                        format!(
                            "{} {} Iris changes",
                            symbols::PREVIEW,
                            status.iris_unsettled
                        ),
                        dim_style(),
                    )],
                    vec![Span::styled(
                        Self::file_hint(&status.iris_paths, 2),
                        dim_style(),
                    )],
                    false,
                    width,
                ));
            }
            if status.user_dirty > 0 {
                lines.push(menu_row(
                    false,
                    vec![
                        Span::styled(
                            format!("{}{}", symbols::DIRTY, status.user_dirty),
                            prompt_style(),
                        ),
                        Span::styled(" yours (protected)".to_string(), Style::default()),
                    ],
                    vec![Span::styled(
                        Self::file_hint(&status.user_paths, 2),
                        dim_style(),
                    )],
                    false,
                    width,
                ));
            }
        }

        // SWITCH group.
        let rows = self.rows();
        let branches = self.switch_branches();
        let shown = branches.len().min(SWITCH_CAP);
        if shown > 0 {
            lines.push(group_label("SWITCH"));
        }
        for (row_index, &branch_index) in branches.iter().take(SWITCH_CAP).enumerate() {
            let info = &status.recent_branches[branch_index];
            let current = Some(info.name.as_str()) == status.branch.as_deref();
            let mut label = vec![Span::raw(info.name.clone())];
            if info.worktree.is_some() {
                label.push(Span::styled(" [WT]".to_string(), dim_style()));
            }
            let meta = if current {
                vec![Span::styled("here".to_string(), dim_style())]
            } else if let Some(path) = &info.worktree {
                vec![Span::styled(home_rel(path), dim_style())]
            } else if let Some(age) = info.age {
                vec![Span::styled(compact_age(age), dim_style())]
            } else {
                Vec::new()
            };
            lines.push(menu_row(
                current,
                label,
                meta,
                selecting && self.selected == row_index,
                width,
            ));
        }
        if branches.len() > SWITCH_CAP {
            lines.push(menu_row(
                false,
                vec![Span::styled(
                    format!("… {} more", branches.len() - SWITCH_CAP),
                    dim_style(),
                )],
                vec![Span::styled("/ to filter".to_string(), dim_style())],
                false,
                width,
            ));
        }

        // WORKTREES group (omitted with only the main worktree).
        if status.worktrees.len() > 1 {
            lines.push(group_label("WORKTREES"));
            for (index, wt) in status.worktrees.iter().take(WORKTREE_CAP).enumerate() {
                let row_index = shown + index;
                let mut meta: Vec<Span<'static>> = Vec::new();
                if let Some(branch) = &wt.branch {
                    meta.push(Span::styled(branch.clone(), dim_style()));
                }
                if let Some(badge) = &wt.unsettled {
                    meta.push(Span::styled(
                        format!(
                            " · {} unsettled · {}",
                            symbols::PREVIEW,
                            compact_age(badge.age)
                        ),
                        dim_style(),
                    ));
                }
                lines.push(menu_row(
                    wt.is_current,
                    vec![Span::raw(home_rel(&wt.path))],
                    meta,
                    selecting && self.selected == row_index && row_index < rows.len(),
                    width,
                ));
            }
        }

        // Readonly / confirm / create states dim the list above the footer.
        if readonly || in_footer_state {
            dim_lines(&mut lines);
        }

        lines.push(internal_rule(width));
        if readonly {
            lines.push(readonly_footer(width));
            return lines;
        }
        match &self.mode {
            Mode::List => {
                let mut items: Vec<(&str, &str)> = Vec::new();
                if status.task.is_some() {
                    items.push(("a", "accept"));
                    items.push(("r", "roll back"));
                }
                items.extend([
                    ("↑↓", "move"),
                    ("↵", "switch"),
                    ("n", "new branch"),
                    ("w", "new worktree"),
                    ("/", "filter"),
                    ("esc", ""),
                ]);
                lines.push(footer_hints(&items, width));
            }
            Mode::Confirm(confirm) => lines.push(self.confirm_footer(confirm, width)),
            Mode::Create {
                input,
                base,
                worktree,
            } => {
                // Two rows: the group label with the base (and resolved path
                // for a worktree), then the input row.
                let label = if *worktree {
                    format!(
                        "NEW WORKTREE — from {base} · at {}",
                        home_rel(&self.worktree_path(if input.is_empty() {
                            "<name>"
                        } else {
                            input
                        }))
                    )
                } else {
                    format!("NEW BRANCH — from {base}")
                };
                let rule = lines.pop().expect("internal rule present");
                lines.push(group_label(&label));
                lines.push(rule);
                let invalid = !input.is_empty() && self.validate_name(input).is_err();
                let mut hint: Vec<Span<'static>> = Vec::new();
                if invalid {
                    hint.push(Span::styled(
                        format!("{} invalid name ", symbols::ERROR),
                        err_style(),
                    ));
                } else {
                    hint.push(Span::styled("↵ create ".to_string(), dim_style()));
                }
                let target = if *worktree {
                    "tab target: branch ⇄ WORKTREE"
                } else {
                    "tab target: BRANCH ⇄ worktree"
                };
                hint.push(Span::styled(
                    format!("{} {target} {} esc", symbols::SEP, symbols::SEP),
                    dim_style(),
                ));
                lines.push(input_row(input, invalid, hint, width));
            }
            Mode::Filter { input, .. } => {
                let count = self.filter_matches(input).len();
                let hint = vec![Span::styled(
                    format!(
                        "{} {} ↵ top {} esc",
                        match_count(count),
                        symbols::SEP,
                        symbols::SEP
                    ),
                    dim_style(),
                )];
                lines.push(input_row(input, false, hint, width));
            }
            Mode::WorktreeReady { path } => {
                lines.push(footer_hints(
                    &[
                        (
                            "",
                            &format!("{} worktree ready at {}", symbols::DONE, home_rel(path)),
                        ),
                        ("↵", "open session there"),
                        ("esc", "stay"),
                    ],
                    width,
                ));
            }
            Mode::Rollback { .. } => unreachable!("rollback renders its own list"),
        }
        lines
    }

    fn confirm_footer(&self, confirm: &Confirm, width: usize) -> Line<'static> {
        match confirm {
            Confirm::TaskUnsettled { .. } => footer_hints(
                &[
                    (
                        "",
                        &format!(
                            "{} task unsettled ({} Iris changes)",
                            symbols::REVIEW,
                            self.status.iris_unsettled
                        ),
                    ),
                    ("a", "accept"),
                    ("r", "roll back"),
                    ("↵", "carry anyway"),
                    ("esc", ""),
                ],
                width,
            ),
            Confirm::Dirty { .. } => footer_hints(
                &[
                    (
                        "",
                        &format!(
                            "{} {} uncommitted files",
                            symbols::REVIEW,
                            self.status.total_uncommitted
                        ),
                    ),
                    ("↵", "carry changes"),
                    ("s", "stash first"),
                    ("esc", "cancel"),
                ],
                width,
            ),
            Confirm::WorktreeRedirect { branch, path } => {
                let lead = match branch {
                    Some(branch) => {
                        format!("{branch} is checked out at {}", home_rel(path))
                    }
                    None => format!("worktree at {}", home_rel(path)),
                };
                footer_hints(
                    &[("", &lead), ("↵", "open session there"), ("esc", "")],
                    width,
                )
            }
            Confirm::Conflicts => footer_hints(
                &[
                    (
                        "",
                        &format!(
                            "{} {} conflicts — resolve before switching",
                            symbols::REVIEW,
                            self.status.unmerged
                        ),
                    ),
                    ("/diff", "review"),
                    ("esc", ""),
                ],
                width,
            ),
            Confirm::Detached { .. } => footer_hints(
                &[
                    ("n", "new branch from HEAD"),
                    ("↵", "switch (leaves HEAD)"),
                    ("esc", ""),
                ],
                width,
            ),
        }
    }

    fn render_rollback(
        &self,
        width: usize,
        points: &[(u64, String)],
        selected: usize,
    ) -> Vec<Line<'static>> {
        let mut lines = vec![self.status_line(width), group_label("ROLL BACK TO")];
        for (index, (seq, label)) in points.iter().enumerate() {
            lines.push(menu_row(
                index == selected,
                vec![Span::raw(label.clone())],
                vec![Span::styled(format!("seq {seq}"), dim_style())],
                index == selected,
                width,
            ));
        }
        lines.push(internal_rule(width));
        lines.push(footer_hints(
            &[("↑↓", "move"), ("↵", "restore"), ("esc", "back")],
            width,
        ));
        lines
    }
}

/// Local approximation of `git check-ref-format --branch` (per-keystroke
/// validation; git itself is the final arbiter at create time).
pub(crate) fn valid_branch_name(name: &str) -> bool {
    if name.is_empty() || name == "@" {
        return false;
    }
    if name.starts_with('/')
        || name.ends_with('/')
        || name.starts_with('.')
        || name.ends_with('.')
        || name.ends_with(".lock")
        || name.starts_with('-')
    {
        return false;
    }
    if name.contains("..") || name.contains("@{") || name.contains("//") || name.contains("/.") {
        return false;
    }
    !name.chars().any(|c| {
        c.is_control() || c.is_whitespace() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::git::status::{BranchInfo, TaskSummary, WorktreeInfo};

    use super::super::lines_text;
    use super::*;

    fn branch(name: &str) -> BranchInfo {
        BranchInfo {
            name: name.to_string(),
            age: Some(Duration::from_secs(2 * 86_400)),
            worktree: None,
        }
    }

    fn base_status() -> GitStatus {
        GitStatus {
            branch: Some("main".to_string()),
            upstream: Some("origin/main".to_string()),
            recent_branches: vec![branch("main"), branch("feat/x"), branch("fix/y")],
            worktrees: vec![WorktreeInfo {
                path: PathBuf::from("/repo"),
                branch: Some("main".to_string()),
                is_current: true,
                unsettled: None,
            }],
            last_commit_age: Some(Duration::from_secs(3 * 3600)),
            ..GitStatus::default()
        }
    }

    fn task_status() -> GitStatus {
        GitStatus {
            total_uncommitted: 5,
            iris_unsettled: 3,
            user_dirty: 2,
            staged: 1,
            untracked: 3,
            iris_paths: vec![
                "src/ui/tui/screen.rs".to_string(),
                "src/ui/tui/startup.rs".to_string(),
                "src/x.rs".to_string(),
            ],
            user_paths: vec!["src/cli.rs".to_string(), "src/main.rs".to_string()],
            task: Some(TaskSummary {
                task_id: "46b10456deadbeef".to_string(),
                age: Duration::from_secs(3 * 3600),
            }),
            ..base_status()
        }
    }

    fn menu(status: GitStatus) -> GitMenu {
        GitMenu::new(status, PathBuf::from("/wt"))
    }

    #[test]
    fn rest_state_renders_status_task_switch_and_footer() {
        let m = menu(task_status());
        let text = lines_text(&m.render_lines(80, 16, false));
        assert!(text.contains("main → origin/main"), "{text}");
        assert!(text.contains("±2 yours · 1 staged · 3 untracked"), "{text}");
        assert!(text.contains("3h ago"), "{text}");
        assert!(
            text.contains("TASK — unsettled · 46b10456 · 3h ago"),
            "{text}"
        );
        assert!(text.contains("◇ 3 Iris changes"), "{text}");
        assert!(text.contains("screen.rs · startup.rs · +1 more"), "{text}");
        assert!(text.contains("±2 yours (protected)"), "{text}");
        assert!(text.contains("SWITCH"), "{text}");
        assert!(text.contains("◉ main"), "{text}");
        assert!(text.contains("here"), "{text}");
        assert!(text.contains("a accept ┊ r roll back"), "{text}");
        // Only one worktree: no WORKTREES group.
        assert!(!text.contains("WORKTREES"), "{text}");
    }

    #[test]
    fn clean_status_line_says_clean_and_omits_task_group() {
        let m = menu(base_status());
        let text = lines_text(&m.render_lines(80, 16, false));
        assert!(text.contains("clean"), "{text}");
        assert!(!text.contains("TASK"), "{text}");
        assert!(!text.contains("a accept"), "{text}");
    }

    #[test]
    fn switch_selection_moves_and_wraps() {
        let mut m = menu(base_status());
        assert_eq!(m.handle_key(MenuKey::Down, false), MenuOutcome::Redraw);
        assert_eq!(m.selected, 1);
        assert_eq!(m.handle_key(MenuKey::Up, false), MenuOutcome::Redraw);
        assert_eq!(m.handle_key(MenuKey::Up, false), MenuOutcome::Redraw);
        assert_eq!(m.selected, 2, "wraps to the last row");
    }

    #[test]
    fn enter_on_clean_branch_emits_checkout() {
        let mut m = menu(base_status());
        m.handle_key(MenuKey::Down, false);
        let out = m.handle_key(MenuKey::Enter, false);
        assert_eq!(
            out,
            MenuOutcome::Action(MenuAction::Checkout {
                branch: "feat/x".to_string()
            })
        );
    }

    #[test]
    fn enter_with_task_confirms_settlement_first() {
        let mut m = menu(task_status());
        m.handle_key(MenuKey::Down, false);
        assert_eq!(m.handle_key(MenuKey::Enter, false), MenuOutcome::Redraw);
        let text = lines_text(&m.render_lines(80, 16, false));
        assert!(text.contains("▲ task unsettled (3 Iris changes)"), "{text}");
        assert!(text.contains("carry anyway"), "{text}");
        // a = settle-first switch.
        let out = m.handle_key(MenuKey::Char('a'), false);
        assert_eq!(
            out,
            MenuOutcome::Action(MenuAction::AcceptThenCheckout {
                branch: "feat/x".to_string()
            })
        );
    }

    #[test]
    fn enter_when_dirty_without_task_offers_carry_or_stash() {
        let mut m = menu(GitStatus {
            total_uncommitted: 3,
            user_dirty: 3,
            modified: 3,
            ..base_status()
        });
        m.handle_key(MenuKey::Down, false);
        assert_eq!(m.handle_key(MenuKey::Enter, false), MenuOutcome::Redraw);
        let text = lines_text(&m.render_lines(80, 16, false));
        assert!(text.contains("▲ 3 uncommitted files"), "{text}");
        assert!(text.contains("s stash first"), "{text}");
        let out = m.handle_key(MenuKey::Char('s'), false);
        assert_eq!(
            out,
            MenuOutcome::Action(MenuAction::StashCheckout {
                branch: "feat/x".to_string()
            })
        );
    }

    #[test]
    fn conflicts_disable_switching_entirely() {
        let mut m = menu(GitStatus {
            unmerged: 2,
            total_uncommitted: 2,
            user_dirty: 2,
            ..base_status()
        });
        m.handle_key(MenuKey::Down, false);
        m.handle_key(MenuKey::Enter, false);
        let text = lines_text(&m.render_lines(80, 16, false));
        assert!(
            text.contains("▲ 2 conflicts — resolve before switching"),
            "{text}"
        );
        // Enter in the conflicts state does nothing.
        assert_eq!(m.handle_key(MenuKey::Enter, false), MenuOutcome::Ignore);
    }

    #[test]
    fn wt_branch_redirects_to_open_session_there() {
        let mut status = base_status();
        status.recent_branches[1].worktree = Some(PathBuf::from("/wt/split"));
        let mut m = menu(status);
        m.handle_key(MenuKey::Down, false);
        m.handle_key(MenuKey::Enter, false);
        let text = lines_text(&m.render_lines(80, 16, false));
        assert!(text.contains("feat/x is checked out at"), "{text}");
        let out = m.handle_key(MenuKey::Enter, false);
        assert_eq!(
            out,
            MenuOutcome::Action(MenuAction::OpenSessionAt {
                path: PathBuf::from("/wt/split"),
                branch: Some("feat/x".to_string())
            })
        );
    }

    #[test]
    fn worktree_rows_render_with_task_badge() {
        let mut status = task_status();
        status.worktrees.push(WorktreeInfo {
            path: PathBuf::from("/wt/split"),
            branch: Some("feat/split".to_string()),
            is_current: false,
            unsettled: Some(crate::git::status::TaskBadge {
                files: 2,
                age: Duration::from_secs(3600),
            }),
        });
        let m = menu(status);
        let text = lines_text(&m.render_lines(90, 16, false));
        assert!(text.contains("WORKTREES"), "{text}");
        assert!(text.contains("◇ unsettled · 1h"), "{text}");
    }

    #[test]
    fn accept_and_rollback_route_to_settlement_actions() {
        let mut m = menu(task_status());
        assert_eq!(
            m.handle_key(MenuKey::Char('a'), false),
            MenuOutcome::Action(MenuAction::Accept)
        );
        assert_eq!(
            m.handle_key(MenuKey::Char('r'), false),
            MenuOutcome::Action(MenuAction::LoadRestorePoints)
        );
        // No task: single letters are inert.
        let mut clean = menu(base_status());
        assert_eq!(
            clean.handle_key(MenuKey::Char('a'), false),
            MenuOutcome::Ignore
        );
    }

    #[test]
    fn rollback_sublist_renders_and_restores_by_seq() {
        let mut m = menu(task_status());
        m.set_restore_points(vec![
            (0, "pre-task baseline".to_string()),
            (1, "edit screen.rs".to_string()),
            (2, "edit startup.rs (+2 more)".to_string()),
        ]);
        let text = lines_text(&m.render_lines(80, 16, false));
        assert!(text.contains("ROLL BACK TO"), "{text}");
        assert!(text.contains("pre-task baseline"), "{text}");
        assert!(text.contains("seq 0"), "{text}");
        assert!(text.contains("↵ restore"), "{text}");
        m.handle_key(MenuKey::Down, false);
        let out = m.handle_key(MenuKey::Enter, false);
        assert_eq!(out, MenuOutcome::Action(MenuAction::Rollback { seq: 1 }));
    }

    #[test]
    fn create_branch_validates_and_toggles_target_with_tab() {
        let mut m = menu(base_status());
        m.handle_key(MenuKey::Char('n'), false);
        assert!(m.input_active());
        for c in "feat/new".chars() {
            m.handle_key(MenuKey::Char(c), false);
        }
        let text = lines_text(&m.render_lines(80, 16, false));
        assert!(text.contains("NEW BRANCH — from main"), "{text}");
        assert!(text.contains("▋feat/new"), "{text}");
        assert!(text.contains("tab target"), "{text}");
        // Tab flips to worktree target with the resolved path visible.
        m.handle_key(MenuKey::Tab, false);
        let text = lines_text(&m.render_lines(90, 16, false));
        assert!(text.contains("NEW WORKTREE"), "{text}");
        assert!(text.contains("feat-new"), "path preview: {text}");
        let out = m.handle_key(MenuKey::Enter, false);
        assert_eq!(
            out,
            MenuOutcome::Action(MenuAction::CreateWorktree {
                name: "feat/new".to_string(),
                base: "main".to_string(),
                path: PathBuf::from("/wt/feat-new"),
            })
        );
    }

    #[test]
    fn invalid_or_colliding_name_gates_enter() {
        let mut m = menu(base_status());
        m.handle_key(MenuKey::Char('n'), false);
        for c in "bad..name".chars() {
            m.handle_key(MenuKey::Char(c), false);
        }
        let text = lines_text(&m.render_lines(80, 16, false));
        assert!(text.contains("■ invalid name"), "{text}");
        assert_eq!(m.handle_key(MenuKey::Enter, false), MenuOutcome::Ignore);
        // Collision with an existing branch is invalid too.
        let mut m = menu(base_status());
        m.handle_key(MenuKey::Char('n'), false);
        for c in "main".chars() {
            m.handle_key(MenuKey::Char(c), false);
        }
        assert_eq!(m.handle_key(MenuKey::Enter, false), MenuOutcome::Ignore);
    }

    #[test]
    fn single_letter_commands_are_text_inside_inputs() {
        let mut m = menu(task_status());
        m.handle_key(MenuKey::Char('/'), false);
        assert!(m.input_active());
        // `a` filters instead of accepting.
        assert_eq!(m.handle_key(MenuKey::Char('a'), false), MenuOutcome::Redraw);
        let text = lines_text(&m.render_lines(80, 16, false));
        assert!(text.contains("▋a"), "{text}");
    }

    #[test]
    fn filter_narrows_and_enter_acts_on_top_match() {
        let mut m = menu(base_status());
        m.handle_key(MenuKey::Char('/'), false);
        for c in "fix".chars() {
            m.handle_key(MenuKey::Char(c), false);
        }
        let text = lines_text(&m.render_lines(80, 16, false));
        assert!(
            text.contains("1 match") && !text.contains("1 matches"),
            "{text}"
        );
        let out = m.handle_key(MenuKey::Enter, false);
        assert_eq!(
            out,
            MenuOutcome::Action(MenuAction::Checkout {
                branch: "fix/y".to_string()
            })
        );
    }

    #[test]
    fn readonly_blocks_every_mutating_key() {
        let mut m = menu(task_status());
        for key in [
            MenuKey::Enter,
            MenuKey::Char('a'),
            MenuKey::Char('r'),
            MenuKey::Char('n'),
            MenuKey::Char('w'),
            MenuKey::Char('s'),
        ] {
            assert_eq!(m.handle_key(key, true), MenuOutcome::Ignore, "{key:?}");
        }
        assert_eq!(m.handle_key(MenuKey::Down, true), MenuOutcome::Redraw);
        assert_eq!(m.handle_key(MenuKey::Esc, true), MenuOutcome::Close);
        let text = lines_text(&m.render_lines(80, 16, true));
        assert!(
            text.contains("● agent running ┊ read-only — actions return when idle ┊ esc"),
            "{text}"
        );
    }

    #[test]
    fn worktree_ready_confirm_offers_open_or_stay() {
        let mut m = menu(base_status());
        m.worktree_ready(PathBuf::from("/wt/feat-new"));
        let text = lines_text(&m.render_lines(90, 16, false));
        assert!(text.contains("◆ worktree ready at"), "{text}");

        // `esc stay` returns to the list (console stays open), it does not
        // close the whole console.
        let mut stayed = menu(base_status());
        stayed.worktree_ready(PathBuf::from("/wt/feat-new"));
        assert_eq!(stayed.handle_key(MenuKey::Esc, false), MenuOutcome::Redraw);
        assert!(matches!(stayed.mode, Mode::List));

        let out = m.handle_key(MenuKey::Enter, false);
        assert_eq!(
            out,
            MenuOutcome::Action(MenuAction::OpenSessionAt {
                path: PathBuf::from("/wt/feat-new"),
                branch: None
            })
        );
    }

    #[test]
    fn detached_promotes_new_branch_from_head() {
        let mut m = menu(GitStatus {
            branch: None,
            detached_at: Some("46b104 fix: meter pulse".to_string()),
            ..base_status()
        });
        let text = lines_text(&m.render_lines(90, 16, false));
        assert!(text.contains("■ detached at 46b104"), "{text}");
        m.handle_key(MenuKey::Enter, false);
        let text = lines_text(&m.render_lines(90, 16, false));
        assert!(text.contains("n new branch from HEAD"), "{text}");
        assert!(text.contains("↵ switch (leaves HEAD)"), "{text}");
        m.handle_key(MenuKey::Char('n'), false);
        for c in "hotfix".chars() {
            m.handle_key(MenuKey::Char(c), false);
        }
        let out = m.handle_key(MenuKey::Enter, false);
        assert_eq!(
            out,
            MenuOutcome::Action(MenuAction::CreateBranch {
                name: "hotfix".to_string(),
                base: "HEAD".to_string()
            })
        );
    }

    #[test]
    fn rows_never_overflow_the_width() {
        use super::super::super::wrap::{display_width, line_text};
        let m = menu(task_status());
        for width in [10usize, 24, 40, 80] {
            for line in m.render_lines(width, 16, false) {
                assert!(
                    display_width(&line_text(&line)) <= width,
                    "width {width}: {:?}",
                    line_text(&line)
                );
            }
        }
    }

    #[test]
    fn height_cap_pins_rule_and_footer() {
        let m = menu(task_status());
        let lines = m.render_lines(80, 6, false);
        assert_eq!(lines.len(), 6);
        let text = lines_text(&lines);
        assert!(text.contains("╌╌"), "{text}");
        assert!(text.contains("↵ switch"), "{text}");
    }

    #[test]
    fn valid_branch_name_rules() {
        assert!(valid_branch_name("feat/git-dropdown"));
        assert!(valid_branch_name("hotfix"));
        for bad in [
            "",
            "feat/bad..name",
            "-lead",
            ".hidden",
            "trail.",
            "a b",
            "x~y",
            "x^y",
            "x:y",
            "x?y",
            "x*y",
            "x[y",
            "x\\y",
            "a//b",
            "a/.b",
            "end.lock",
            "/lead",
            "trail/",
            "@",
            "a@{b",
        ] {
            assert!(!valid_branch_name(bad), "{bad:?} should be invalid");
        }
    }
}
