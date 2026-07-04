//! The directory-tree dropdown: breadcrumb, lazily-expanded tree rows with
//! task-attribution markers, and the flat fuzzy filter (`/` or `@`-entry).
//!
//! Data: `git ls-files --cached --others --exclude-standard` when the root is
//! a repo (respects .gitignore), plain readdir otherwise. `↵` on a file
//! inserts `@<relative-path> ` into the composer; `↵` on a dir toggles it.
//! No box-drawing tree guides — indent + `▾`/`▸` carry the structure.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ratatui::text::{Line, Span};

use crate::git::status::GitStatus;
use crate::ui::symbols;
use crate::wayland::git_safety::git;

use super::super::wrap::truncate_line;
use super::super::{dim_style, prompt_style};
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
    /// Directory listing cache (key = dir rel path, `""` = root).
    children: BTreeMap<String, Vec<Entry>>,
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
            children: BTreeMap::new(),
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
        self.children.clear();
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
    /// files, both alphabetical. Cached.
    fn children_of(&mut self, dir: &str) -> Vec<Entry> {
        if let Some(cached) = self.children.get(dir) {
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
        self.children.insert(dir.to_string(), entries.clone());
        entries
    }

    /// Files under a dir prefix (collapsed-dir meta), git mode only.
    fn files_under(&self, dir: &str) -> Option<usize> {
        let files = self.files.as_ref()?;
        let prefix = format!("{dir}/");
        Some(files.iter().filter(|f| f.starts_with(&prefix)).count())
    }

    /// Build the visible rows for the current expansion state (capped).
    fn visible_rows(&mut self) -> (Vec<VisRow>, usize) {
        let mut rows = Vec::new();
        let mut overflow = 0usize;
        let mut stack: Vec<(String, usize)> = vec![(String::new(), 0)];
        // Depth-first walk of expanded dirs.
        fn walk(
            menu: &mut TreeMenu,
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
        let (root, depth) = stack.pop().expect("root frame");
        walk(self, &root, depth, &mut rows, &mut overflow);
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

    /// The attribution marker meta for a path, from the status partition.
    fn attribution(
        &self,
        rel: &str,
        dir: bool,
        git: Option<&GitStatus>,
        referenced: &[String],
    ) -> Vec<Span<'static>> {
        // Markers only apply while the root is the session cwd (paths align).
        if self.root != self.cwd {
            return Vec::new();
        }
        if !dir && referenced.iter().any(|r| r == rel) {
            return vec![Span::styled(
                format!("{} open", symbols::ACTIVE),
                prompt_style(),
            )];
        }
        let Some(status) = git else {
            return Vec::new();
        };
        let prefix = format!("{rel}/");
        let has = |paths: &[String]| {
            if dir {
                paths.iter().any(|p| p.starts_with(&prefix))
            } else {
                paths.iter().any(|p| p == rel)
            }
        };
        if has(&status.iris_paths) {
            return vec![Span::styled(
                format!("{} iris", symbols::PREVIEW),
                dim_style(),
            )];
        }
        if has(&status.user_paths) {
            let style = if dir { dim_style() } else { prompt_style() };
            return vec![Span::styled(format!("{} yours", symbols::DIRTY), style)];
        }
        Vec::new()
    }

    pub(crate) fn render_lines(
        &self,
        width: usize,
        max_rows: usize,
        readonly: bool,
        git: Option<&GitStatus>,
        referenced: &[String],
    ) -> Vec<Line<'static>> {
        // `visible_rows` needs `&mut` for the listing cache; clone the state
        // cheaply instead of threading interior mutability through render.
        let mut walker = TreeMenu {
            root: self.root.clone(),
            cwd: self.cwd.clone(),
            files: self.files.clone(),
            children: self.children.clone(),
            expanded: self.expanded.clone(),
            selected: self.selected,
            mode: self.mode.clone(),
        };
        let mut lines = vec![self.breadcrumb(width)];
        match &self.mode {
            Mode::Browse => {
                let (rows, overflow) = walker.visible_rows();
                for (index, row) in rows.iter().enumerate() {
                    lines.push(self.tree_row(
                        row,
                        index == self.selected && !readonly,
                        width,
                        git,
                        referenced,
                        &walker,
                    ));
                }
                if overflow > 0 {
                    lines.push(Line::from(Span::styled(
                        format!("   … {overflow} more"),
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
    /// name (dirs ink + `/`, files stdout grey) · right-aligned dim meta.
    fn tree_row(
        &self,
        row: &VisRow,
        selected: bool,
        width: usize,
        git: Option<&GitStatus>,
        referenced: &[String],
        walker: &TreeMenu,
    ) -> Line<'static> {
        let indent = " ".repeat(row.depth * 2 + 1);
        let mut label: Vec<Span<'static>> = vec![Span::raw(indent)];
        if row.entry.dir {
            let glyph = if self.expanded.contains(&row.entry.rel) {
                symbols::EXPANDED
            } else {
                symbols::COLLAPSED
            };
            label.push(Span::styled(format!("{glyph} "), dim_style()));
            label.push(Span::raw(format!("{}/", row.entry.name)));
        } else {
            label.push(Span::styled(
                row.entry.name.clone(),
                ratatui::style::Style::default().fg(ratatui::style::Color::Gray),
            ));
        }
        let mut meta = self.attribution(&row.entry.rel, row.entry.dir, git, referenced);
        if meta.is_empty()
            && row.entry.dir
            && !self.expanded.contains(&row.entry.rel)
            && let Some(count) = walker.files_under(&row.entry.rel)
        {
            let noun = if count == 1 { "file" } else { "files" };
            meta = vec![Span::styled(format!("{count} {noun}"), dim_style())];
        }
        // Tree rows use plain gap alignment, not the dotted leader (the tree
        // is denser than a picker; leaders would draw a wall of dots).
        let label_w: usize = label.iter().map(|s| super::display_width(&s.content)).sum();
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
            children: BTreeMap::new(),
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
        let mut t = tree(FILES);
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
        let mut t = tree(&refs);
        let (rows, overflow) = t.visible_rows();
        assert_eq!(rows.len(), 500);
        assert_eq!(overflow, 100);
        let text = lines_text(&t.render_lines(40, 1000, false, None, &[]));
        assert!(text.contains("… 100 more"), "{text}");
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
