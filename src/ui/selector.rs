//! Reusable selector state (Tier 3, presentation-only, TTY-free).
//!
//! A single list-with-search primitive shared by every picker in [`super::modal`]
//! (model, scoped-models, effort, settings, login method/provider). It owns the
//! item list, the search string, the highlighted row, and the filtered view, and
//! exposes a small windowing helper so the renderer only has to draw rows. It
//! holds no terminal handle and performs no I/O, so the whole state machine is
//! unit-tested without a TTY.
//!
//! Filtering is a case-insensitive subsequence (fuzzy) match over each item's
//! `filter` haystack -- the same "type the letters in order" feel as pi-mono's
//! fuzzy filter, without pulling in a fuzzy-matching dependency for what a few
//! lines of stdlib cover.

/// One selectable row. `id` is the caller's stable key (e.g. `provider/model`);
/// the rest is display-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectorItem {
    pub(crate) id: String,
    /// Primary label.
    pub(crate) label: String,
    /// Secondary, dimmer text shown after the label (provider badge, a level
    /// description, an auth status, ...).
    pub(crate) detail: Option<String>,
    /// Trailing marker drawn at the row end (e.g. the current-model `✓`).
    pub(crate) trailing: Option<String>,
    /// Optional enabled-column glyph for checkmark lists (scoped-models). `None`
    /// hides the column entirely.
    pub(crate) enabled: Option<bool>,
    /// Lowercased haystack used for fuzzy filtering.
    filter: String,
}

impl SelectorItem {
    pub(crate) fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        let id = id.into();
        let label = label.into();
        let filter = format!("{id} {label}").to_ascii_lowercase();
        Self {
            id,
            label,
            detail: None,
            trailing: None,
            enabled: None,
            filter,
        }
    }

    pub(crate) fn detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        self.filter.push(' ');
        self.filter.push_str(&detail.to_ascii_lowercase());
        self.detail = Some(detail);
        self
    }

    pub(crate) fn trailing(mut self, trailing: impl Into<String>) -> Self {
        self.trailing = Some(trailing.into());
        self
    }

    pub(crate) fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }
}

/// A list-with-search picker. `searchable` toggles the search row; `wrap`
/// chooses wrap-around vs clamp at the list boundaries (pi-mono wraps the model
/// list but clamps the provider list); `window` is the max rows shown at once.
#[derive(Debug, Clone)]
pub(crate) struct Selector {
    items: Vec<SelectorItem>,
    filtered: Vec<usize>,
    cursor: usize,
    search: Option<String>,
    wrap: bool,
    window: usize,
}

/// What a single visible row carries to the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisibleRow<'a> {
    pub(crate) item: &'a SelectorItem,
    pub(crate) selected: bool,
}

impl Selector {
    pub(crate) fn new(
        items: Vec<SelectorItem>,
        searchable: bool,
        wrap: bool,
        window: usize,
    ) -> Self {
        let mut selector = Self {
            items,
            filtered: Vec::new(),
            cursor: 0,
            search: searchable.then(String::new),
            wrap,
            window: window.max(1),
        };
        selector.refilter(None);
        selector
    }

    /// Replace the item list (e.g. after a scoped-models toggle/reorder),
    /// preserving the search string and keeping the highlight on the same id
    /// when it still exists.
    pub(crate) fn replace_items(&mut self, items: Vec<SelectorItem>) {
        let keep = self.selected().map(|item| item.id.clone());
        self.items = items;
        self.refilter(keep.as_deref());
    }

    /// Recompute the filtered view. When `keep_id` is set and still visible after
    /// filtering, the cursor is moved onto it; otherwise the cursor clamps into
    /// range (0 for a fresh filter).
    fn refilter(&mut self, keep_id: Option<&str>) {
        let needle = self.search.as_deref().unwrap_or("").to_ascii_lowercase();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| fuzzy_match(&needle, &item.filter))
            .map(|(index, _)| index)
            .collect();
        self.cursor = keep_id
            .and_then(|id| {
                self.filtered
                    .iter()
                    .position(|&index| self.items[index].id == id)
            })
            .unwrap_or(0);
        self.clamp_cursor();
    }

    fn clamp_cursor(&mut self) {
        if self.filtered.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.filtered.len() {
            self.cursor = self.filtered.len() - 1;
        }
    }

    /// Move the highlight to the row with `id` if it is currently visible.
    pub(crate) fn select_id(&mut self, id: &str) {
        if let Some(position) = self
            .filtered
            .iter()
            .position(|&index| self.items[index].id == id)
        {
            self.cursor = position;
        }
    }

    pub(crate) fn up(&mut self) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        self.cursor = if self.cursor == 0 {
            if self.wrap { len - 1 } else { 0 }
        } else {
            self.cursor - 1
        };
    }

    pub(crate) fn down(&mut self) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        self.cursor = if self.cursor + 1 >= len {
            if self.wrap { 0 } else { len - 1 }
        } else {
            self.cursor + 1
        };
    }

    pub(crate) fn selected(&self) -> Option<&SelectorItem> {
        self.filtered
            .get(self.cursor)
            .map(|&index| &self.items[index])
    }

    pub(crate) fn selected_id(&self) -> Option<&str> {
        self.selected().map(|item| item.id.as_str())
    }

    pub(crate) fn searchable(&self) -> bool {
        self.search.is_some()
    }

    pub(crate) fn search(&self) -> Option<&str> {
        self.search.as_deref()
    }

    /// Insert a typed character into the search field and re-filter. No-op when
    /// the selector is not searchable.
    pub(crate) fn push_char(&mut self, c: char) {
        if let Some(search) = self.search.as_mut() {
            search.push(c);
            self.refilter(None);
        }
    }

    /// Delete the last search character and re-filter. Returns whether anything
    /// changed (so a caller can decide whether a bare backspace should bubble up).
    pub(crate) fn backspace(&mut self) -> bool {
        if let Some(search) = self.search.as_mut()
            && search.pop().is_some()
        {
            self.refilter(None);
            return true;
        }
        false
    }

    /// Clear the search field (pi-mono Ctrl+C-clears-search). Returns whether the
    /// search was non-empty (so the caller can fall through to cancel when empty).
    pub(crate) fn clear_search(&mut self) -> bool {
        match self.search.as_mut() {
            Some(search) if !search.is_empty() => {
                search.clear();
                self.refilter(None);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.filtered.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn filtered_count(&self) -> usize {
        self.filtered.len()
    }

    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    /// First filtered index shown given the window, scrolled to keep the cursor
    /// visible.
    fn scroll_offset(&self) -> usize {
        if self.cursor < self.window {
            0
        } else {
            self.cursor - self.window + 1
        }
    }

    /// Whether the list is scrolled (more rows than the window).
    pub(crate) fn is_scrolled(&self) -> bool {
        self.filtered.len() > self.window
    }

    /// The `(selectedIndex/filteredCount)` position label pi-mono shows when the
    /// list is scrolled. 1-based selected index.
    pub(crate) fn position_label(&self) -> String {
        format!("({}/{})", self.cursor + 1, self.filtered.len())
    }

    /// The visible window of rows, with the selected flag set on the cursor row.
    pub(crate) fn visible(&self) -> Vec<VisibleRow<'_>> {
        let offset = self.scroll_offset();
        self.filtered
            .iter()
            .enumerate()
            .skip(offset)
            .take(self.window)
            .map(|(position, &index)| VisibleRow {
                item: &self.items[index],
                selected: position == self.cursor,
            })
            .collect()
    }

    /// All currently filtered items (used by Ctrl+A/Ctrl+X "all matching" ops).
    pub(crate) fn filtered_ids(&self) -> Vec<String> {
        self.filtered
            .iter()
            .map(|&index| self.items[index].id.clone())
            .collect()
    }
}

/// Case-insensitive subsequence match: every char of `needle` (already
/// lowercased) appears in `haystack` (already lowercased) in order. An empty
/// needle matches everything.
fn fuzzy_match(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|target| chars.any(|candidate| candidate == target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<SelectorItem> {
        vec![
            SelectorItem::new("openai-codex/gpt-5.5", "gpt-5.5").detail("openai-codex"),
            SelectorItem::new("anthropic/claude-sonnet-4-6", "claude-sonnet-4-6")
                .detail("anthropic"),
            SelectorItem::new("antigravity/gemini-3.5-flash", "gemini-3.5-flash")
                .detail("antigravity"),
        ]
    }

    #[test]
    fn fuzzy_subsequence_matches_in_order() {
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("gpt", "gpt-5.5 openai-codex"));
        assert!(fuzzy_match("oai", "openai-codex"));
        assert!(!fuzzy_match("zzz", "openai-codex"));
        // Out-of-order chars do not match.
        assert!(!fuzzy_match("tpg", "gpt"));
    }

    #[test]
    fn search_filters_and_resets_cursor() {
        let mut selector = Selector::new(items(), true, true, 10);
        assert_eq!(selector.filtered_count(), 3);
        for c in "claude".chars() {
            selector.push_char(c);
        }
        assert_eq!(selector.filtered_count(), 1);
        assert_eq!(selector.selected_id(), Some("anthropic/claude-sonnet-4-6"));
        // Backspacing widens the result again.
        assert!(selector.backspace());
        assert_eq!(selector.search(), Some("claud"));
    }

    #[test]
    fn up_down_wrap_when_enabled_and_clamp_when_not() {
        let mut wrapping = Selector::new(items(), false, true, 10);
        assert_eq!(wrapping.cursor(), 0);
        wrapping.up();
        assert_eq!(wrapping.cursor(), 2, "wrap to last from first");
        wrapping.down();
        assert_eq!(wrapping.cursor(), 0, "wrap to first from last");

        let mut clamping = Selector::new(items(), false, false, 10);
        clamping.up();
        assert_eq!(clamping.cursor(), 0, "clamp at top");
        clamping.down();
        clamping.down();
        clamping.down();
        assert_eq!(clamping.cursor(), 2, "clamp at bottom");
    }

    #[test]
    fn replace_items_keeps_cursor_on_same_id() {
        let mut selector = Selector::new(items(), true, true, 10);
        selector.down(); // anthropic
        assert_eq!(selector.selected_id(), Some("anthropic/claude-sonnet-4-6"));
        // Reorder: move anthropic to the front; cursor should follow it.
        let mut reordered = items();
        reordered.swap(0, 1);
        selector.replace_items(reordered);
        assert_eq!(selector.selected_id(), Some("anthropic/claude-sonnet-4-6"));
    }

    #[test]
    fn windowing_scrolls_to_keep_cursor_visible() {
        let many: Vec<SelectorItem> = (0..20)
            .map(|i| SelectorItem::new(format!("id-{i}"), format!("label-{i}")))
            .collect();
        let mut selector = Selector::new(many, false, false, 5);
        assert!(!selector.is_scrolled() || selector.visible().len() == 5);
        for _ in 0..7 {
            selector.down();
        }
        let visible = selector.visible();
        assert_eq!(visible.len(), 5);
        // The selected (cursor=7) row is within the window.
        assert!(visible.iter().any(|row| row.selected));
        assert!(selector.is_scrolled());
        assert_eq!(selector.position_label(), "(8/20)");
    }

    #[test]
    fn clear_search_reports_whether_it_was_non_empty() {
        let mut selector = Selector::new(items(), true, true, 10);
        assert!(!selector.clear_search(), "empty search returns false");
        selector.push_char('x');
        assert!(
            selector.clear_search(),
            "non-empty search clears and returns true"
        );
        assert_eq!(selector.search(), Some(""));
        assert_eq!(selector.filtered_count(), 3);
    }
}
