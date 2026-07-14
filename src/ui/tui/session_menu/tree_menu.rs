//! The directory-tree dropdown: breadcrumb, lazily-expanded tree rows with
//! task-attribution markers, and the flat fuzzy filter (`/` or `@`-entry).
//!
//! Data: `git ls-files --cached --others --exclude-standard` when the root is
//! a repo (respects .gitignore), plain readdir otherwise. `↵` on a file
//! inserts `@<relative-path> ` into the composer; `↵` on a dir toggles it.
//! No box-drawing tree guides — indent + `▾`/`▸` carry the structure.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ratatui::text::{Line, Span};

use crate::git::status::GitStatus;
use crate::ui::symbols;
use crate::wayland::git_safety::git;

use super::super::wrap::truncate_line;
use super::super::{dim_style, prompt_style, stdout_style};
use super::{
    MenuAction, MenuKey, MenuOutcome, cap_block, dim_lines, footer_hints, fuzzy_match, home_rel,
    input_row, internal_rule, match_count, menu_row, readonly_footer, step_wrapped,
};

/// Hard cap on visible rows (a dim `… N more` row follows).
const VISIBLE_ROW_CAP: usize = 500;
/// Cap on the loaded file list (very large repos).
const FILE_CAP: usize = 20_000;

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    /// Path relative to the tree root (`src/ui/tui`).
    rel: String,
    name: String,
    dir: bool,
}

/// One visible row: an entry at a depth.
#[derive(Debug, Clone)]
struct VisRow {
    entry: Entry,
    depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Browse,
    Filter { input: String, selected: usize },
}

/// Directory-tree state.
pub(crate) struct TreeMenu {
    root: PathBuf,
    /// The session cwd, so inserted references stay cwd-relative even after a
    /// breadcrumb re-root.
    cwd: PathBuf,
    /// Repo file list relative to `root` (`None` = non-repo readdir mode).
    files: Option<Vec<String>>,
    /// Directory listing cache (key = dir rel path, `""` = root). Behind a
    /// `RefCell` so the read-only render path can fill it in place — the tree
    /// is never cloned per frame to obtain `&mut` for the cache.
    children: RefCell<BTreeMap<String, Vec<Entry>>>,
    expanded: BTreeSet<String>,
    selected: usize,
    mode: Mode,
}

impl TreeMenu {
    /// Open the tree at `cwd`. `filter` = open directly in filter mode (the
    /// `@`-entry idiom).
    pub(crate) fn new(cwd: PathBuf, filter: bool) -> Self {
        let mut menu = Self {
            root: cwd.clone(),
            cwd,
            files: None,
            children: RefCell::new(BTreeMap::new()),
            expanded: BTreeSet::new(),
            selected: 0,
            mode: if filter {
                Mode::Filter {
                    input: String::new(),
                    selected: 0,
                }
            } else {
                Mode::Browse
            },
        };
        menu.reload();
        menu
    }

    #[cfg(test)]
    pub(crate) fn input_active(&self) -> bool {
        matches!(self.mode, Mode::Filter { .. })
    }

    /// (Re)load the file list for the current root.
    fn reload(&mut self) {
        self.children.get_mut().clear();
        self.expanded.clear();
        self.selected = 0;
        self.files = git::is_git_worktree(&self.root)
            .then(|| {
                git::git_stdout(
                    &self.root,
                    &[
                        "ls-files",
                        "--cached",
                        "--others",
                        "--exclude-standard",
                        "-z",
                    ],
                )
                .ok()
                .map(|bytes| {
                    bytes
                        .split(|&b| b == 0)
                        .filter(|t| !t.is_empty())
                        .take(FILE_CAP)
                        .map(|t| String::from_utf8_lossy(t).into_owned())
                        .collect::<Vec<String>>()
                })
            })
            .flatten();
    }

    /// Re-root one level up (breadcrumb click).
    pub(crate) fn reroot_up(&mut self) -> bool {
        let Some(parent) = self.root.parent().map(Path::to_path_buf) else {
            return false;
        };
        self.root = parent;
        self.reload();
        true
    }

    /// Immediate children of `dir` (rel path, `""` = root), dirs first then
    /// files, both alphabetical. Cached through the `RefCell` so rendering can
    /// fill the cache without a `&mut` borrow of the tree.
    fn children_of(&self, dir: &str) -> Vec<Entry> {
        if let Some(cached) = self.children.borrow().get(dir) {
            return cached.clone();
        }
        let entries: Vec<Entry> = match &self.files {
            Some(files) => {
                let prefix = if dir.is_empty() {
                    String::new()
                } else {
                    format!("{dir}/")
                };
                let mut dirs: BTreeSet<String> = BTreeSet::new();
                let mut plain: BTreeSet<String> = BTreeSet::new();
                for file in files {
                    let Some(rest) = file.strip_prefix(&prefix) else {
                        continue;
                    };
                    match rest.split_once('/') {
                        Some((child, _)) => {
                            dirs.insert(child.to_string());
                        }
                        None if !rest.is_empty() => {
                            plain.insert(rest.to_string());
                        }
                        None => {}
                    }
                }
                let join = |name: &str| {
                    if dir.is_empty() {
                        name.to_string()
                    } else {
                        format!("{dir}/{name}")
                    }
                };
                dirs.iter()
                    .map(|name| Entry {
                        rel: join(name),
                        name: name.clone(),
                        dir: true,
                    })
                    .chain(plain.iter().map(|name| Entry {
                        rel: join(name),
                        name: name.clone(),
                        dir: false,
                    }))
                    .collect()
            }
            None => {
                // Plain readdir (non-repo); `.git` and hidden entries skipped.
                let abs = if dir.is_empty() {
                    self.root.clone()
                } else {
                    self.root.join(dir)
                };
                let mut dirs = Vec::new();
                let mut plain = Vec::new();
                if let Ok(read) = std::fs::read_dir(&abs) {
                    for entry in read.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if name.starts_with('.') {
                            continue;
                        }
                        let rel = if dir.is_empty() {
                            name.clone()
                        } else {
                            format!("{dir}/{name}")
                        };
                        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                        if is_dir {
                            dirs.push(Entry {
                                rel,
                                name,
                                dir: true,
                            });
                        } else {
                            plain.push(Entry {
                                rel,
                                name,
                                dir: false,
                            });
                        }
                    }
                }
                dirs.sort_by(|a, b| a.name.cmp(&b.name));
                plain.sort_by(|a, b| a.name.cmp(&b.name));
                dirs.into_iter().chain(plain).collect()
            }
        };
        self.children
            .borrow_mut()
            .insert(dir.to_string(), entries.clone());
        entries
    }

    /// Files under a dir prefix (collapsed-dir meta), git mode only.
    fn files_under(&self, dir: &str) -> Option<usize> {
        let files = self.files.as_ref()?;
        let prefix = format!("{dir}/");
        Some(files.iter().filter(|f| f.starts_with(&prefix)).count())
    }

    /// Build the visible rows for the current expansion state (capped). Takes
    /// `&self`: the listing cache lives behind a `RefCell`, so the render path
    /// walks the tree without cloning it for a `&mut` borrow.
    fn visible_rows(&self) -> (Vec<VisRow>, usize) {
        let mut rows = Vec::new();
        let mut overflow = 0usize;
        // Depth-first walk of expanded dirs.
        fn walk(
            menu: &TreeMenu,
            dir: &str,
            depth: usize,
            rows: &mut Vec<VisRow>,
            overflow: &mut usize,
        ) {
            for entry in menu.children_of(dir) {
                if rows.len() >= VISIBLE_ROW_CAP {
                    *overflow += 1;
                    continue;
                }
                let expanded = entry.dir && menu.expanded.contains(&entry.rel);
                rows.push(VisRow {
                    entry: entry.clone(),
                    depth,
                });
                if expanded {
                    walk(menu, &entry.rel, depth + 1, rows, overflow);
                }
            }
        }
        walk(self, "", 0, &mut rows, &mut overflow);
        (rows, overflow)
    }

    fn filter_matches(&self, input: &str) -> Vec<String> {
        match &self.files {
            Some(files) => files
                .iter()
                .filter(|file| fuzzy_match(input, file))
                .take(VISIBLE_ROW_CAP)
                .cloned()
                .collect(),
            None => Vec::new(),
        }
    }

    /// The `@`-reference text for a rel path: cwd-relative when possible.
    fn reference_for(&self, rel: &str) -> String {
        let full = self.root.join(rel);
        match full.strip_prefix(&self.cwd) {
            Ok(relative) => relative.display().to_string(),
            Err(_) => full.display().to_string(),
        }
    }

    // --- keys -----------------------------------------------------------

    pub(crate) fn handle_key(&mut self, key: MenuKey, readonly: bool) -> MenuOutcome {
        if readonly {
            return match key {
                MenuKey::Esc => MenuOutcome::Close,
                MenuKey::Up => self.move_selection(-1),
                MenuKey::Down => self.move_selection(1),
                _ => MenuOutcome::Ignore,
            };
        }
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::Browse => self.key_browse(key),
            Mode::Filter { input, selected } => self.key_filter(input, selected, key),
        }
    }

    fn move_selection(&mut self, delta: isize) -> MenuOutcome {
        let (rows, _) = self.visible_rows();
        if rows.is_empty() {
            return MenuOutcome::Ignore;
        }
        self.selected = step_wrapped(self.selected, rows.len(), delta);
        MenuOutcome::Redraw
    }

    fn key_browse(&mut self, key: MenuKey) -> MenuOutcome {
        match key {
            MenuKey::Esc => MenuOutcome::Close,
            MenuKey::Up => self.move_selection(-1),
            MenuKey::Down => self.move_selection(1),
            MenuKey::Right => {
                let (rows, _) = self.visible_rows();
                let Some(row) = rows.get(self.selected) else {
                    return MenuOutcome::Ignore;
                };
                if !row.entry.dir {
                    return MenuOutcome::Ignore;
                }
                if self.expanded.insert(row.entry.rel.clone()) {
                    MenuOutcome::Redraw
                } else {
                    // Already expanded: step into the first child.
                    self.move_selection(1)
                }
            }
            MenuKey::Left => {
                let (rows, _) = self.visible_rows();
                let Some(row) = rows.get(self.selected).cloned() else {
                    return MenuOutcome::Ignore;
                };
                if row.entry.dir && self.expanded.remove(&row.entry.rel) {
                    return MenuOutcome::Redraw;
                }
                // Step to the parent row.
                if let Some(parent) = row.entry.rel.rsplit_once('/').map(|(p, _)| p.to_string())
                    && let Some(index) = rows.iter().position(|r| r.entry.rel == parent)
                {
                    self.selected = index;
                    return MenuOutcome::Redraw;
                }
                MenuOutcome::Ignore
            }
            MenuKey::Enter => {
                let (rows, _) = self.visible_rows();
                let Some(row) = rows.get(self.selected).cloned() else {
                    return MenuOutcome::Ignore;
                };
                if row.entry.dir {
                    if !self.expanded.remove(&row.entry.rel) {
                        self.expanded.insert(row.entry.rel.clone());
                    }
                    MenuOutcome::Redraw
                } else {
                    MenuOutcome::Action(MenuAction::InsertReference(
                        self.reference_for(&row.entry.rel),
                    ))
                }
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

    fn key_filter(&mut self, mut input: String, mut selected: usize, key: MenuKey) -> MenuOutcome {
        match key {
            MenuKey::Esc => {
                self.mode = Mode::Browse;
                return MenuOutcome::Redraw;
            }
            MenuKey::Up | MenuKey::Down => {
                let count = self.filter_matches(&input).len();
                if count > 0 {
                    let delta: isize = if key == MenuKey::Up { -1 } else { 1 };
                    selected = step_wrapped(selected, count, delta);
                }
            }
            MenuKey::Enter => {
                let matches = self.filter_matches(&input);
                if let Some(rel) = matches.get(selected.min(matches.len().saturating_sub(1))) {
                    return MenuOutcome::Action(MenuAction::InsertReference(
                        self.reference_for(rel),
                    ));
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

    /// Mouse click on a dropdown line: the breadcrumb (line 0) re-roots one
    /// level up; a tree row is selected on first click and activated (like
    /// `↵`) on the second.
    pub(crate) fn click_line(&mut self, line: usize, readonly: bool) -> MenuOutcome {
        if !matches!(self.mode, Mode::Browse) {
            return MenuOutcome::Ignore;
        }
        if line == 0 {
            if readonly {
                return MenuOutcome::Ignore;
            }
            return if self.reroot_up() {
                MenuOutcome::Redraw
            } else {
                MenuOutcome::Ignore
            };
        }
        let index = line - 1;
        let (rows, _) = self.visible_rows();
        if index >= rows.len() {
            return MenuOutcome::Ignore;
        }
        if self.selected == index {
            if readonly {
                return MenuOutcome::Ignore;
            }
            return self.key_browse(MenuKey::Enter);
        }
        self.selected = index;
        MenuOutcome::Redraw
    }

    // --- render -----------------------------------------------------------

    /// Breadcrumb: parents dim (clickable re-root up), current bold.
    fn breadcrumb(&self, width: usize) -> Line<'static> {
        let display = home_rel(&self.root);
        let parts: Vec<&str> = display.split('/').filter(|p| !p.is_empty()).collect();
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" / ".to_string(), dim_style()));
            }
            if index + 1 == parts.len() {
                spans.push(Span::styled(
                    (*part).to_string(),
                    ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled((*part).to_string(), dim_style()));
            }
        }
        let mut line = Line::from(spans);
        truncate_line(&mut line, width.max(1));
        line
    }

    /// Convert a row path (relative to the current `root`) back to a
    /// cwd-relative path — the frame the status partition and composer
    /// references live in — so attribution survives a breadcrumb re-root.
    /// `None` = the row lives outside the cwd subtree (degrade that row only,
    /// never all-or-nothing).
    fn cwd_rel(&self, rel: &str) -> Option<String> {
        self.root
            .join(rel)
            .strip_prefix(&self.cwd)
            .ok()
            .map(|relative| relative.display().to_string())
    }

    /// The attribution marker meta for a FILE, from the status partition:
    /// `◉ open` (composer-referenced), `◇ iris` (unsettled ledger), `± yours`
    /// (user-dirty). Collapsed directories carry the [`Self::dir_rollup`]
    /// instead. Empty for a row outside the cwd subtree.
    fn attribution(
        &self,
        rel: &str,
        git: Option<&GitStatus>,
        referenced: &[String],
    ) -> Vec<Span<'static>> {
        // Markers describe the FILE, not the viewpoint: compare in the
        // cwd-relative frame so attribution renders at any re-root. A row
        // outside the cwd subtree has no cwd-relative form — no marker there.
        let Some(rel) = self.cwd_rel(rel) else {
            return Vec::new();
        };
        let rel = rel.as_str();
        if referenced.iter().any(|r| r == rel) {
            return vec![Span::styled(
                format!("{} open", symbols::ACTIVE),
                prompt_style(),
            )];
        }
        let Some(status) = git else {
            return Vec::new();
        };
        if status.iris_paths.iter().any(|p| p == rel) {
            return vec![Span::styled(
                format!("{} iris", symbols::PREVIEW),
                dim_style(),
            )];
        }
        if status.user_paths.iter().any(|p| p == rel) {
            return vec![Span::styled(
                format!("{} yours", symbols::DIRTY),
                prompt_style(),
            )];
        }
        Vec::new()
    }

    /// Collapsed-directory rollup: the §9.1 state cluster computed from the
    /// already-fetched status path sets — `±N` (orange) user-dirty files
    /// beneath, `◇M` (muted) iris-ledger files beneath, each half omitted at
    /// zero — plus the muted file-count tail. Returned as `(state, count)` so
    /// the row can drop the count first under width pressure: state outranks
    /// inventory (§9.1.1). No new git calls — a prefix-match over the sets the
    /// session bar already fetched.
    fn dir_rollup(
        &self,
        rel: &str,
        git: Option<&GitStatus>,
    ) -> (Vec<Span<'static>>, Option<Span<'static>>) {
        let mut state: Vec<Span<'static>> = Vec::new();
        // An ancestor-of-cwd dir (`cwd_rel` = None: the cwd is inside it, not
        // the other way round) degrades to count-only: the status sets are
        // cwd-scoped, so a rollup there would report only the cwd's slice of a
        // larger subtree — a partial `±N` presented as the dir's state would
        // lie.
        if let Some(status) = git
            && let Some(cwd_rel) = self.cwd_rel(rel)
        {
            // Empty `cwd_rel` = this row IS the session cwd itself (seen from
            // a root above it): every cwd-relative status path lies beneath
            // it, so the prefix matches all.
            let prefix = if cwd_rel.is_empty() {
                String::new()
            } else {
                format!("{cwd_rel}/")
            };
            let under = |paths: &[String]| paths.iter().filter(|p| p.starts_with(&prefix)).count();
            let user = under(&status.user_paths);
            let iris = under(&status.iris_paths);
            if user > 0 {
                state.push(Span::styled(
                    format!("{}{user}", symbols::DIRTY),
                    prompt_style(),
                ));
            }
            if iris > 0 {
                if !state.is_empty() {
                    state.push(Span::raw(" ".to_string()));
                }
                state.push(Span::styled(
                    format!("{}{iris}", symbols::PREVIEW),
                    dim_style(),
                ));
            }
        }
        let count = self.files_under(rel).map(|n| {
            let noun = if n == 1 { "file" } else { "files" };
            // The `·` joins state to inventory only when state is present
            // (`±3 ◇1 · 41 files`); a clean dir reads `41 files` alone.
            let joiner = if state.is_empty() { "" } else { " · " };
            Span::styled(format!("{joiner}{n} {noun}"), dim_style())
        });
        (state, count)
    }

    pub(crate) fn render_lines(
        &self,
        width: usize,
        max_rows: usize,
        readonly: bool,
        git: Option<&GitStatus>,
        referenced: &[String],
    ) -> Vec<Line<'static>> {
        // The listing cache is interior-mutable (`RefCell`), so rendering
        // borrows the tree and fills the cache in place — no per-frame clone.
        let mut lines = vec![self.breadcrumb(width)];
        match &self.mode {
            Mode::Browse => {
                let (rows, overflow) = self.visible_rows();
                for (index, row) in rows.iter().enumerate() {
                    lines.push(self.tree_row(
                        row,
                        index == self.selected && !readonly,
                        width,
                        git,
                        referenced,
                    ));
                }
                if overflow > 0 {
                    // Mirror the git console's SWITCH overflow affordance: the
                    // cap row names the way to reach the elided rows.
                    lines.push(Line::from(Span::styled(
                        format!("   … {overflow} more · / to filter"),
                        dim_style(),
                    )));
                }
                if readonly {
                    dim_lines(&mut lines);
                }
                lines.push(internal_rule(width));
                if readonly {
                    lines.push(readonly_footer(width));
                } else {
                    lines.push(footer_hints(
                        &[
                            ("↑↓", "move"),
                            ("→ ←", "expand / collapse"),
                            ("↵", "reference in composer"),
                            ("/", "filter"),
                            ("esc", ""),
                        ],
                        width,
                    ));
                }
            }
            Mode::Filter { input, selected } => {
                let matches = self.filter_matches(input);
                for (index, rel) in matches.iter().take(max_rows.saturating_sub(3)).enumerate() {
                    let name = rel.rsplit('/').next().unwrap_or(rel.as_str());
                    let parent = rel
                        .rsplit_once('/')
                        .map(|(p, _)| format!("{p}/"))
                        .unwrap_or_default();
                    lines.push(menu_row(
                        false,
                        vec![Span::raw(name.to_string())],
                        vec![Span::styled(parent, dim_style())],
                        index == *selected && !readonly,
                        width,
                    ));
                }
                if readonly {
                    dim_lines(&mut lines);
                }
                lines.push(internal_rule(width));
                if readonly {
                    lines.push(readonly_footer(width));
                } else {
                    let hint = vec![Span::styled(
                        format!(
                            "{} {} ↵ top {} esc",
                            match_count(matches.len()),
                            symbols::SEP,
                            symbols::SEP
                        ),
                        dim_style(),
                    )];
                    lines.push(input_row(input, false, hint, width));
                }
            }
        }
        for line in &mut lines {
            truncate_line(line, width.max(1));
        }
        cap_block(lines, max_rows)
    }

    /// One tree row: 2-cells-per-level indent · disclosure (dirs only, dim) ·
    /// name (dirs ink + `/`, files stdout grey) · right-aligned dim meta. A
    /// file's meta is its attribution marker; a COLLAPSED directory's is the
    /// state rollup (`±N ◇M · N files`); an EXPANDED directory carries none
    /// (its children speak for themselves).
    fn tree_row(
        &self,
        row: &VisRow,
        selected: bool,
        width: usize,
        git: Option<&GitStatus>,
        referenced: &[String],
    ) -> Line<'static> {
        let indent = " ".repeat(row.depth * 2 + 1);
        let mut label: Vec<Span<'static>> = vec![Span::raw(indent)];
        let expanded = self.expanded.contains(&row.entry.rel);
        if row.entry.dir {
            let glyph = if expanded {
                symbols::EXPANDED
            } else {
                symbols::COLLAPSED
            };
            label.push(Span::styled(format!("{glyph} "), dim_style()));
            label.push(Span::raw(format!("{}/", row.entry.name)));
        } else {
            label.push(Span::styled(row.entry.name.clone(), stdout_style()));
        }
        // A collapsed dir rolls up its dirty state + inventory; the count is
        // droppable, the state is not. Files carry a single attribution marker.
        let (mut meta, count): (Vec<Span<'static>>, Option<Span<'static>>) = match &row.entry {
            entry if entry.dir && !expanded => self.dir_rollup(&entry.rel, git),
            entry if entry.dir => (Vec::new(), None),
            entry => (self.attribution(&entry.rel, git, referenced), None),
        };
        // Tree rows use plain gap alignment, not the dotted leader (the tree
        // is denser than a picker; leaders would draw a wall of dots).
        let label_w: usize = label.iter().map(|s| super::display_width(&s.content)).sum();
        // The count tail joins only when the row still holds a 1-col gap after
        // it; otherwise it drops and the state stands alone (§9.1.1).
        if let Some(count) = count {
            let state_w: usize = meta.iter().map(|s| super::display_width(&s.content)).sum();
            let count_w = super::display_width(&count.content);
            // `<` (not `<=`) reserves the 1-col gap between label and meta.
            if label_w + state_w + count_w < width {
                meta.push(count);
            }
        }
        let meta_w: usize = meta.iter().map(|s| super::display_width(&s.content)).sum();
        let gap = width.saturating_sub(label_w).saturating_sub(meta_w);
        let mut spans = label;
        if meta_w > 0 && gap > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
            spans.extend(meta);
        }
        if selected {
            for span in &mut spans {
                span.style = span
                    .style
                    .patch(ratatui::style::Style::default().bg(crate::ui::palette::surface()));
            }
            if let Some(name) = spans.get_mut(1) {
                name.style = name.style.add_modifier(ratatui::style::Modifier::BOLD);
            }
        }
        let mut line = Line::from(spans);
        truncate_line(&mut line, width.max(1));
        line
    }
}

#[cfg(test)]
mod tests {
    use super::super::lines_text;
    use super::*;

    /// A tree with a fixed file list (git mode, no subprocess).
    fn tree(files: &[&str]) -> TreeMenu {
        TreeMenu {
            root: PathBuf::from("/repo"),
            cwd: PathBuf::from("/repo"),
            files: Some(files.iter().map(|f| f.to_string()).collect()),
            children: RefCell::new(BTreeMap::new()),
            expanded: BTreeSet::new(),
            selected: 0,
            mode: Mode::Browse,
        }
    }

    const FILES: &[&str] = &[
        "Cargo.toml",
        "readme.md",
        "docs/spec.md",
        "src/main.rs",
        "src/ui/cli.rs",
        "src/ui/tui/screen.rs",
        "src/ui/tui/startup.rs",
    ];

    #[test]
    fn root_listing_sorts_dirs_first_then_files() {
        let t = tree(FILES);
        let (rows, overflow) = t.visible_rows();
        assert_eq!(overflow, 0);
        let names: Vec<&str> = rows.iter().map(|r| r.entry.name.as_str()).collect();
        assert_eq!(names, vec!["docs", "src", "Cargo.toml", "readme.md"]);
        assert!(rows[0].entry.dir && !rows[2].entry.dir);
    }

    #[test]
    fn lazy_expand_reveals_children_and_collapse_hides_them() {
        let mut t = tree(FILES);
        t.handle_key(MenuKey::Down, false); // select src/
        assert_eq!(t.handle_key(MenuKey::Right, false), MenuOutcome::Redraw);
        let (rows, _) = t.visible_rows();
        let names: Vec<&str> = rows.iter().map(|r| r.entry.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["docs", "src", "ui", "main.rs", "Cargo.toml", "readme.md"]
        );
        assert_eq!(rows[2].depth, 1);
        // Left collapses.
        assert_eq!(t.handle_key(MenuKey::Left, false), MenuOutcome::Redraw);
        let (rows, _) = t.visible_rows();
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn enter_on_file_inserts_reference_and_enter_on_dir_toggles() {
        let mut t = tree(FILES);
        // Select Cargo.toml (row 2).
        t.handle_key(MenuKey::Down, false);
        t.handle_key(MenuKey::Down, false);
        assert_eq!(
            t.handle_key(MenuKey::Enter, false),
            MenuOutcome::Action(MenuAction::InsertReference("Cargo.toml".to_string()))
        );
        // Enter on a dir toggles expansion instead.
        t.selected = 0;
        assert_eq!(t.handle_key(MenuKey::Enter, false), MenuOutcome::Redraw);
        assert!(t.expanded.contains("docs"));
    }

    #[test]
    fn filter_matches_flat_with_parent_meta_and_enter_references_top() {
        let mut t = tree(FILES);
        t.handle_key(MenuKey::Char('/'), false);
        assert!(t.input_active());
        for c in "s.rs".chars() {
            t.handle_key(MenuKey::Char(c), false);
        }
        let text = lines_text(&t.render_lines(60, 16, false, None, &[]));
        assert!(text.contains("screen.rs"), "{text}");
        assert!(text.contains("src/ui/tui/"), "{text}");
        assert!(text.contains("matches"), "{text}");
        assert!(text.contains("▋s.rs"), "{text}");
        let out = t.handle_key(MenuKey::Enter, false);
        assert_eq!(
            out,
            MenuOutcome::Action(MenuAction::InsertReference("src/main.rs".to_string()))
        );
    }

    #[test]
    fn at_filter_finds_nested_files_outside_a_git_worktree() {
        let root = std::env::temp_dir().join(format!(
            "iris-tree-filter-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("src/ui")).unwrap();
        std::fs::write(root.join("src/ui/screen.rs"), "fn main() {}\n").unwrap();

        let mut tree = TreeMenu::new(root.clone(), true);
        for character in "screen".chars() {
            tree.handle_key(MenuKey::Char(character), false);
        }

        let text = lines_text(&tree.render_lines(60, 16, false, None, &[]));
        assert!(text.contains("screen.rs"), "{text}");
        assert!(text.contains("src/ui/"), "{text}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn attribution_markers_render_from_the_status_partition() {
        let mut t = tree(FILES);
        // Expand src/ and src/ui/ so the marked files are visible.
        t.expanded.insert("src".to_string());
        t.expanded.insert("src/ui".to_string());
        let status = GitStatus {
            iris_paths: vec!["src/ui/cli.rs".to_string()],
            user_paths: vec!["src/main.rs".to_string()],
            ..GitStatus::default()
        };
        let text =
            lines_text(&t.render_lines(60, 20, false, Some(&status), &["Cargo.toml".to_string()]));
        assert!(text.contains("◇ iris"), "{text}");
        assert!(text.contains("± yours"), "{text}");
        assert!(text.contains("◉ open"), "{text}");
    }

    #[test]
    fn attribution_survives_reroot_above_cwd() {
        // Re-rooted one level up: `root = /`, session cwd = `/repo`, files
        // listed relative to the new root. Markers describe the file, so a
        // dirty file and a referenced file keep their metas even though the
        // viewpoint moved above the cwd.
        let mut t = TreeMenu {
            root: PathBuf::from("/"),
            cwd: PathBuf::from("/repo"),
            files: Some(vec![
                "repo/src/main.rs".to_string(),
                "repo/Cargo.toml".to_string(),
                "other/README.md".to_string(),
            ]),
            children: RefCell::new(BTreeMap::new()),
            expanded: BTreeSet::new(),
            selected: 0,
            mode: Mode::Browse,
        };
        t.expanded.insert("repo".to_string());
        t.expanded.insert("repo/src".to_string());
        // Status/reference paths stay cwd-relative (`src/main.rs`,
        // `Cargo.toml`); the row paths are root-relative (`repo/...`).
        let status = GitStatus {
            user_paths: vec!["src/main.rs".to_string()],
            ..GitStatus::default()
        };
        let text =
            lines_text(&t.render_lines(60, 20, false, Some(&status), &["Cargo.toml".to_string()]));
        assert!(
            text.contains("± yours"),
            "dirty file keeps its marker: {text}"
        );
        assert!(
            text.contains("◉ open"),
            "referenced file keeps its marker: {text}"
        );
        // A row outside the cwd subtree degrades to no marker, not a panic.
        assert!(
            text.contains("other/"),
            "out-of-cwd row still renders: {text}"
        );
    }

    #[test]
    fn cwd_row_rolls_up_fully_when_rerooted_above() {
        // Re-rooted above the cwd, the cwd's own COLLAPSED dir row has
        // `cwd_rel == ""` — the rollup must match ALL cwd-relative status
        // paths (every dirty file is by definition beneath the cwd), not a
        // bogus `/`-prefix that matches none.
        let t = TreeMenu {
            root: PathBuf::from("/"),
            cwd: PathBuf::from("/repo"),
            files: Some(vec![
                "repo/src/a.rs".to_string(),
                "repo/src/b.rs".to_string(),
                "repo/src/c.rs".to_string(),
                "other/README.md".to_string(),
            ]),
            children: RefCell::new(BTreeMap::new()),
            expanded: BTreeSet::new(),
            selected: 0,
            mode: Mode::Browse,
        };
        // `repo/` stays collapsed; status paths are cwd-relative.
        let status = GitStatus {
            user_paths: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            iris_paths: vec!["src/c.rs".to_string()],
            ..GitStatus::default()
        };
        let lines = t.render_lines(60, 20, false, Some(&status), &[]);
        let repo = row_text(&lines, "repo/");
        assert!(repo.contains("±2 ◇1 · 3 files"), "{repo:?}");
        // An out-of-cwd dir keeps the count-only degrade.
        let other = row_text(&lines, "other/");
        assert!(other.contains("1 file"), "{other:?}");
        assert!(
            !other.contains('±') && !other.contains('◇'),
            "no state on out-of-cwd dir: {other:?}"
        );
    }

    const ROLLUP_FILES: &[&str] = &[
        "docs/guide.md",
        "docs/spec.md",
        "src/a.rs",
        "src/b.rs",
        "src/c.rs",
    ];

    fn rollup_status() -> GitStatus {
        GitStatus {
            user_paths: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            iris_paths: vec!["src/c.rs".to_string()],
            ..GitStatus::default()
        }
    }

    /// The rendered row (below the breadcrumb) whose label starts with `name`.
    fn row_text(lines: &[Line<'static>], name: &str) -> String {
        lines
            .iter()
            .map(super::super::line_text)
            .find(|t| t.trim_start().contains(name))
            .unwrap_or_default()
    }

    #[test]
    fn collapsed_dir_rolls_up_dirty_state_then_count() {
        let t = tree(ROLLUP_FILES);
        let status = rollup_status();
        let lines = t.render_lines(60, 20, false, Some(&status), &[]);
        // src/ collapsed: 2 user-dirty + 1 iris file beneath, then inventory.
        let src = row_text(&lines, "src/");
        assert!(src.contains("±2 ◇1 · 3 files"), "{src:?}");
    }

    #[test]
    fn zero_state_dir_renders_count_only() {
        let t = tree(ROLLUP_FILES);
        let status = rollup_status();
        let lines = t.render_lines(60, 20, false, Some(&status), &[]);
        let docs = row_text(&lines, "docs/");
        assert!(docs.contains("2 files"), "{docs:?}");
        assert!(
            !docs.contains('±') && !docs.contains('◇'),
            "no state glyphs: {docs:?}"
        );
    }

    #[test]
    fn expanded_dir_renders_no_rollup() {
        let mut t = tree(ROLLUP_FILES);
        t.expanded.insert("src".to_string());
        let status = rollup_status();
        let lines = t.render_lines(60, 20, false, Some(&status), &[]);
        let src = row_text(&lines, "src/");
        // The expanded dir row carries neither state nor inventory; its
        // children speak for themselves.
        assert!(!src.contains("files"), "no count on expanded dir: {src:?}");
        assert!(
            !src.contains("±2"),
            "no state rollup on expanded dir: {src:?}"
        );
    }

    #[test]
    fn narrow_width_drops_count_before_state() {
        let t = tree(ROLLUP_FILES);
        let status = rollup_status();
        // Wide: both state and inventory fit.
        let wide = row_text(&t.render_lines(60, 20, false, Some(&status), &[]), "src/");
        assert!(wide.contains("±2 ◇1"), "{wide:?}");
        assert!(wide.contains("files"), "{wide:?}");
        // Narrow: the inventory tail drops, the state cluster survives.
        let narrow = row_text(&t.render_lines(20, 20, false, Some(&status), &[]), "src/");
        assert!(narrow.contains("±2 ◇1"), "state kept: {narrow:?}");
        assert!(!narrow.contains("files"), "count dropped: {narrow:?}");
    }

    #[test]
    fn rollup_is_dim_under_readonly() {
        let t = tree(ROLLUP_FILES);
        let status = rollup_status();
        let lines = t.render_lines(60, 20, true, Some(&status), &[]);
        // The orange `±N` half is recolored to muted like every readout row.
        let dirty = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.starts_with('±'));
        let dirty = dirty.expect("rollup ± span present");
        assert_eq!(
            dirty.style.fg,
            Some(crate::ui::palette::muted()),
            "readonly dims the rollup: {:?}",
            dirty.style
        );
    }

    #[test]
    fn breadcrumb_marks_current_component_bold() {
        let t = tree(FILES);
        let lines = t.render_lines(60, 16, false, None, &[]);
        let crumb = &lines[0];
        assert!(
            crumb
                .spans
                .iter()
                .any(|span| span.content.as_ref() == "repo"
                    && span
                        .style
                        .add_modifier
                        .contains(ratatui::style::Modifier::BOLD)),
            "{crumb:?}"
        );
    }

    #[test]
    fn visible_rows_cap_at_500_with_overflow_row() {
        let files: Vec<String> = (0..600).map(|i| format!("f{i:04}.txt")).collect();
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let t = tree(&refs);
        let (rows, overflow) = t.visible_rows();
        assert_eq!(rows.len(), 500);
        assert_eq!(overflow, 100);
        let text = lines_text(&t.render_lines(40, 1000, false, None, &[]));
        assert!(text.contains("… 100 more"), "{text}");
        // The cap row carries the filter affordance (spec 1.5).
        assert!(text.contains("… 100 more · / to filter"), "{text}");
    }

    #[test]
    fn render_fills_the_shared_cache_without_cloning() {
        // The render path is `&self`: it borrows the tree and fills the
        // interior-mutable listing cache in place. A per-frame clone would
        // discard the populated cache, leaving this tree's own cache empty.
        let t = tree(FILES);
        assert!(t.children.borrow().is_empty(), "cache starts empty");
        let _ = t.render_lines(60, 16, false, None, &[]);
        assert!(
            t.children.borrow().contains_key(""),
            "render populated the tree's own cache — no throwaway clone"
        );
    }

    #[test]
    fn readonly_shows_readout_footer_and_blocks_reference() {
        let mut t = tree(FILES);
        t.handle_key(MenuKey::Down, false);
        t.handle_key(MenuKey::Down, false);
        assert_eq!(t.handle_key(MenuKey::Enter, true), MenuOutcome::Ignore);
        let text = lines_text(&t.render_lines(80, 16, true, None, &[]));
        assert!(
            text.contains("read-only — actions return when idle"),
            "{text}"
        );
    }

    #[test]
    fn rows_never_overflow_width() {
        use super::super::super::wrap::{display_width, line_text};
        let mut t = tree(FILES);
        t.expanded.insert("src".to_string());
        t.expanded.insert("src/ui".to_string());
        t.expanded.insert("src/ui/tui".to_string());
        for width in [8usize, 20, 44, 80] {
            for line in t.render_lines(width, 16, false, None, &[]) {
                assert!(
                    display_width(&line_text(&line)) <= width,
                    "width {width}: {:?}",
                    line_text(&line)
                );
            }
        }
    }
}
