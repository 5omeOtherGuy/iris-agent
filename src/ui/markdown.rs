//! Minimal Markdown -> ratatui `Line` mapper for assistant text (Tier 3).
//!
//! Iris renders streamed/finalized assistant messages as Markdown so headings,
//! emphasis, code, lists, quotes, and tables read the way the model intends.
//! This is a deliberately small mapper over `pulldown-cmark`'s event stream --
//! NOT a full renderer. It covers headings, bold, italic, strikethrough, inline
//! code, fenced code blocks (dimmed + indented by default, with an optional
//! syntax-highlight hook), bullet/ordered/task lists, blockquotes, GFM tables,
//! and visible link destinations.
//!
//! Styling is injected through [`MarkdownTheme`] rather than hardcoded, and a
//! caller may supply a `highlight_code` hook to colorize fenced blocks. OSC-8
//! hyperlink emission and a bundled syntax highlighter are intentionally out of
//! scope (see ROADMAP / the TUI re-architecture spec): this layer provides the
//! link/highlight seams, not their terminal-level implementations.
//!
//! Output is `Vec<Line<'static>>`; the caller wraps non-table lines to the
//! transcript width. Tables are laid out to fit the supplied render width (they
//! are the one block that cannot be re-wrapped line-by-line without corrupting
//! the box), so a width is threaded in for table layout only.

use std::rc::Rc;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Indent applied per Markdown nesting level (list depth / blockquote).
const INDENT: &str = "  ";

/// Fallback render width for table layout when no real terminal width is known
/// yet (e.g. the first paint before any frame has been measured).
pub(crate) const DEFAULT_RENDER_WIDTH: usize = 80;

/// Optional fenced-code colorizer: `(code, lang) -> styled lines`. A caller can
/// inject a syntax highlighter here; Iris ships no built-in implementation, so
/// the default theme leaves it `None` and fenced blocks render dimmed.
pub(crate) type HighlightFn = Rc<dyn Fn(&str, Option<&str>) -> Vec<Line<'static>>>;

/// Injected styling for Markdown elements (the Rust analogue of pi-mono's
/// `MarkdownTheme`/`DefaultTextStyle`). Each field is a ratatui [`Style`] that is
/// `patch`-composed onto `base`, so emphasis stacks (e.g. bold inside a quote).
/// [`MarkdownTheme::default`] reproduces Iris's historical hardcoded styling
/// exactly, so existing output is byte-identical unless a caller overrides it.
#[derive(Clone)]
pub(crate) struct MarkdownTheme {
    /// Base style patched under every emitted span (pi-mono's `defaultTextStyle`).
    /// Used by the thinking block to render the whole trace dim + italic.
    pub(crate) base: Style,
    pub(crate) heading: Style,
    pub(crate) bold: Style,
    pub(crate) italic: Style,
    pub(crate) strikethrough: Style,
    pub(crate) quote: Style,
    pub(crate) inline_code: Style,
    pub(crate) code_block: Style,
    /// Style applied to link label text (the visible destination is preserved
    /// regardless). Default is a no-op so historical output is unchanged.
    pub(crate) link: Style,
    /// Style for the appended `(url)` when a link's label differs from its dest.
    pub(crate) link_url: Style,
    pub(crate) list_bullet: Style,
    pub(crate) rule: Style,
    pub(crate) table_border: Style,
    pub(crate) table_header: Style,
    /// Optional fenced-code highlighter; `None` keeps the dim default.
    pub(crate) highlight_code: Option<HighlightFn>,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        let dim = Style::default().add_modifier(Modifier::DIM);
        Self {
            base: Style::default(),
            heading: Style::default().add_modifier(Modifier::BOLD),
            bold: Style::default().add_modifier(Modifier::BOLD),
            italic: Style::default().add_modifier(Modifier::ITALIC),
            strikethrough: Style::default().add_modifier(Modifier::CROSSED_OUT),
            quote: dim,
            inline_code: Style::default().fg(Color::Cyan),
            code_block: dim,
            link: Style::default(),
            link_url: dim,
            list_bullet: dim,
            rule: dim,
            table_border: dim,
            table_header: Style::default().add_modifier(Modifier::BOLD),
            highlight_code: None,
        }
    }
}

impl MarkdownTheme {
    /// Theme for assistant reasoning ("thinking") traces: the default theme with
    /// a dim + italic base so the whole trace reads as muted thought, mirroring
    /// pi-mono's `thinkingText`/italic default text style.
    pub(crate) fn thinking() -> Self {
        Self {
            base: Style::default()
                .add_modifier(Modifier::DIM)
                .add_modifier(Modifier::ITALIC),
            ..Self::default()
        }
    }
}

/// Render Markdown `text` into styled transcript lines with the default theme at
/// the default layout width. Convenience entry point over
/// [`render_markdown_themed`]; production callers thread a real width/theme, so
/// this is currently only used by tests.
#[cfg(test)]
pub(crate) fn render_markdown(text: &str) -> Vec<Line<'static>> {
    render_markdown_themed(text, &MarkdownTheme::default(), DEFAULT_RENDER_WIDTH)
}

/// Render Markdown with an explicit theme and table-layout width. `width` is the
/// number of columns available for the widest table line; non-table blocks are
/// width-agnostic and wrapped later by the caller.
pub(crate) fn render_markdown_themed(
    text: &str,
    theme: &MarkdownTheme,
    width: usize,
) -> Vec<Line<'static>> {
    let mut renderer = Renderer::new(theme, width.max(1));
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    for event in Parser::new_ext(text, options) {
        renderer.event(event);
    }
    renderer.finish()
}

struct Renderer<'a> {
    theme: &'a MarkdownTheme,
    width: usize,
    out: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    /// Ordered-list counters (`Some(n)`) or bullet markers (`None`), one per
    /// open list, so nesting indents and numbering stay correct.
    lists: Vec<Option<u64>>,
    bold: u32,
    italic: u32,
    strike: u32,
    quote: u32,
    heading: bool,
    in_code_block: bool,
    code_lang: Option<String>,
    code_buf: String,
    links: Vec<LinkState>,
    /// Prefix (indent + marker / quote markers) for the line being built.
    prefix: String,
    table: Option<TableState>,
}

#[derive(Default)]
struct LinkState {
    dest: String,
    label: String,
}

/// Accumulated GFM table state. Cells collect plain text (inline emphasis inside
/// cells is not preserved) so column-width math and cell wrapping stay simple and
/// correct; the table is rendered to box-drawing lines on `TagEnd::Table`.
#[derive(Default)]
struct TableState {
    alignments: Vec<Alignment>,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    cur_row: Vec<String>,
    cur_cell: String,
}

impl<'a> Renderer<'a> {
    fn new(theme: &'a MarkdownTheme, width: usize) -> Self {
        Self {
            theme,
            width,
            out: Vec::new(),
            spans: Vec::new(),
            lists: Vec::new(),
            bold: 0,
            italic: 0,
            strike: 0,
            quote: 0,
            heading: false,
            in_code_block: false,
            code_lang: None,
            code_buf: String::new(),
            links: Vec::new(),
            prefix: String::new(),
            table: None,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => {
                let style = self.theme.base.patch(self.theme.inline_code);
                self.push_span(format!("`{code}`"), style)
            }
            Event::SoftBreak => self.push_span(" ".to_string(), self.text_style()),
            Event::HardBreak => {
                if let Some(table) = &mut self.table {
                    // A hard break inside a cell becomes a space; cells are
                    // single logical strings that the layout wraps per column.
                    table.cur_cell.push(' ');
                } else {
                    self.flush();
                }
            }
            // Horizontal rule: a dim divider line.
            Event::Rule => {
                self.flush();
                self.out
                    .push(Line::from(Span::styled("---", self.theme.rule)));
            }
            // GFM task-list checkbox; arrives at the start of a list item's text.
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                let style = self.theme.base.patch(self.theme.list_bullet);
                self.push_span(marker.to_string(), style);
            }
            // Raw/inline HTML (and angle-bracket text like `<thinking>` that
            // model output often contains): render the literal text rather than
            // dropping it, so nothing silently vanishes from the transcript.
            Event::Html(html) | Event::InlineHtml(html) => self.text(&html),
            // Footnotes, math: nothing to render here.
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { .. } => self.heading = true,
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::BlockQuote(_) => self.quote += 1,
            Tag::CodeBlock(kind) => {
                self.flush();
                self.in_code_block = true;
                self.code_buf.clear();
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => Some(lang.to_string()),
                    _ => None,
                };
            }
            Tag::Link { dest_url, .. } => self.links.push(LinkState {
                dest: dest_url.to_string(),
                label: String::new(),
            }),
            Tag::List(start) => {
                // Flush the parent item's text before its nested list overwrites
                // the line prefix (e.g. "- top" before "  - nested").
                self.flush();
                self.lists.push(start);
            }
            Tag::Item => {
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => "- ".to_string(),
                };
                self.prefix = format!("{}{marker}", INDENT.repeat(depth));
            }
            Tag::Table(alignments) => {
                self.flush();
                self.table = Some(TableState {
                    alignments,
                    ..TableState::default()
                });
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.cur_row = Vec::new();
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.cur_cell = String::new();
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.flush();
                self.heading = false;
                self.blank();
            }
            TagEnd::Paragraph => {
                self.flush();
                self.blank();
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::BlockQuote(_) => self.quote = self.quote.saturating_sub(1),
            TagEnd::CodeBlock => {
                self.flush_code_block();
                self.in_code_block = false;
                self.blank();
            }
            TagEnd::Link => self.end_link(),
            TagEnd::List(_) => {
                self.lists.pop();
                self.prefix = INDENT.repeat(self.lists.len());
                self.blank();
            }
            TagEnd::Item => {
                self.flush();
                self.prefix.clear();
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    let cell = std::mem::take(&mut table.cur_cell);
                    table.cur_row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.header = std::mem::take(&mut table.cur_row);
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.cur_row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.render_table(table);
                    self.blank();
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if self.in_code_block {
            self.code_buf.push_str(text);
            return;
        }
        let style = self.text_style();
        self.push_span(text.to_string(), style);
    }

    fn text_style(&self) -> Style {
        let mut style = self.theme.base;
        if self.heading {
            style = style.patch(self.theme.heading);
        }
        if self.bold > 0 {
            style = style.patch(self.theme.bold);
        }
        if self.italic > 0 {
            style = style.patch(self.theme.italic);
        }
        if self.strike > 0 {
            style = style.patch(self.theme.strikethrough);
        }
        if self.quote > 0 {
            style = style.patch(self.theme.quote);
        }
        style
    }

    fn push_span(&mut self, content: String, style: Style) {
        if content.is_empty() {
            return;
        }
        for link in &mut self.links {
            link.label.push_str(&content);
        }
        // Inside a table cell, collect plain text only (cell styling is applied
        // when the table is rendered).
        if let Some(table) = &mut self.table {
            table.cur_cell.push_str(&content);
            return;
        }
        let style = if self.links.is_empty() {
            style
        } else {
            style.patch(self.theme.link)
        };
        self.spans.push(Span::styled(content, style));
    }

    fn end_link(&mut self) {
        let Some(link) = self.links.pop() else {
            return;
        };
        if link.dest.is_empty() || link.label.contains(&link.dest) {
            return;
        }
        let style = self.theme.base.patch(self.theme.link_url);
        self.push_span(format!(" ({})", link.dest), style);
    }

    fn flush_code_block(&mut self) {
        let prefix = self.code_block_prefix();
        let style = self.theme.base.patch(self.theme.code_block);
        if let Some(highlight) = self.theme.highlight_code.clone() {
            let code = std::mem::take(&mut self.code_buf);
            let code = code.strip_suffix('\n').unwrap_or(&code);
            for mut line in highlight(code, self.code_lang.as_deref()) {
                if !prefix.is_empty() {
                    line.spans.insert(0, Span::styled(prefix.clone(), style));
                }
                self.out.push(line);
            }
            return;
        }
        let code = std::mem::take(&mut self.code_buf);
        // `split` keeps internal blank lines but yields a trailing empty segment
        // for the block's final newline, which we drop.
        let segments: Vec<&str> = code.split('\n').collect();
        let last = segments.len().saturating_sub(1);
        for (i, line) in segments.iter().enumerate() {
            if i == last && line.is_empty() {
                continue;
            }
            self.out
                .push(Line::from(Span::styled(format!("{prefix}{line}"), style)));
        }
    }

    fn code_block_prefix(&self) -> String {
        let mut prefix = "> ".repeat(self.quote as usize);
        prefix.push_str(&Self::continuation_prefix(&self.prefix));
        prefix.push_str("    ");
        prefix
    }

    fn continuation_prefix(prefix: &str) -> String {
        " ".repeat(prefix.chars().count())
    }

    /// Flush the in-progress spans as one line, prepending the indent/quote
    /// marker prefix. A line with only a prefix (e.g. an empty list item) is
    /// still emitted so structure is visible. No-op while collecting a table.
    fn flush(&mut self) {
        if self.table.is_some() {
            return;
        }
        let quote = "> ".repeat(self.quote as usize);
        let prefix = format!("{quote}{}", self.prefix);
        if self.spans.is_empty() && self.prefix.is_empty() {
            return;
        }
        if self.spans.is_empty()
            && !self.prefix.is_empty()
            && self.prefix.chars().all(|ch| ch == ' ')
        {
            return;
        }
        let mut line = Vec::with_capacity(self.spans.len() + 1);
        if !prefix.is_empty() {
            line.push(Span::styled(
                prefix,
                self.theme.base.patch(self.theme.list_bullet),
            ));
        }
        line.append(&mut self.spans);
        self.out.push(Line::from(line));
        if !self.prefix.is_empty() && !self.prefix.chars().all(|ch| ch == ' ') {
            self.prefix = Self::continuation_prefix(&self.prefix);
        }
    }

    /// Push a single blank separator line unless one is already trailing.
    fn blank(&mut self) {
        if self.out.last().is_some_and(|l| line_is_empty(l)) {
            return;
        }
        if self.out.is_empty() {
            return;
        }
        self.out.push(Line::default());
    }

    /// The leading indent for a block (blockquote markers + list-continuation
    /// spaces) so a table nested in a quote/list keeps the document structure,
    /// matching how code blocks are prefixed.
    fn block_prefix(&self) -> String {
        let mut prefix = "> ".repeat(self.quote as usize);
        prefix.push_str(&Self::continuation_prefix(&self.prefix));
        prefix
    }

    /// Render an accumulated GFM table as width-aware box-drawing lines, indented
    /// by the current block prefix (blockquote/list) so nesting is preserved.
    fn render_table(&mut self, table: TableState) {
        let cols = table.header.len();
        if cols == 0 {
            return;
        }
        let border = self.theme.table_border;
        let header_style = self.theme.base.patch(self.theme.table_header);
        let body_style = self.theme.base;

        let prefix = self.block_prefix();
        let prefix_width = UnicodeWidthStr::width(prefix.as_str());
        let prefix_style = self.theme.base.patch(self.theme.list_bullet);
        // Width available to the table itself, after the block indent.
        let table_width = self.width.saturating_sub(prefix_width).max(1);

        // Natural width per column from header + body cells.
        let mut natural = vec![0usize; cols];
        for (i, cell) in table.header.iter().enumerate() {
            natural[i] = natural[i].max(UnicodeWidthStr::width(cell.as_str()));
        }
        for row in &table.rows {
            for (i, cell) in row.iter().take(cols).enumerate() {
                natural[i] = natural[i].max(UnicodeWidthStr::width(cell.as_str()));
            }
        }

        // Border overhead: "│ " + (cols-1) * " │ " + " │" = 3*cols + 1.
        let overhead = cols.saturating_mul(3).saturating_add(1);
        let available = table_width.saturating_sub(overhead);

        let mut lines: Vec<Line<'static>> = Vec::new();
        if available < cols {
            // Too narrow for a stable box: fall back to wrapped pipe-joined rows.
            render_table_fallback(&table, body_style, header_style, table_width, &mut lines);
        } else {
            let widths = fit_columns(&natural, available);
            let make_border = |left: char, mid: char, right: char| -> Line<'static> {
                let mut s = String::new();
                s.push(left);
                for (i, w) in widths.iter().enumerate() {
                    if i > 0 {
                        s.push(mid);
                    }
                    s.push('\u{2500}');
                    s.push_str(&"\u{2500}".repeat(*w));
                    s.push('\u{2500}');
                }
                s.push(right);
                Line::from(Span::styled(s, border))
            };
            lines.push(make_border('\u{250c}', '\u{252c}', '\u{2510}'));
            push_table_row(
                &table.header,
                &widths,
                &table.alignments,
                header_style,
                border,
                &mut lines,
            );
            lines.push(make_border('\u{251c}', '\u{253c}', '\u{2524}'));
            for row in &table.rows {
                push_table_row(
                    row,
                    &widths,
                    &table.alignments,
                    body_style,
                    border,
                    &mut lines,
                );
            }
            lines.push(make_border('\u{2514}', '\u{2534}', '\u{2518}'));
        }

        for mut line in lines {
            if !prefix.is_empty() {
                line.spans
                    .insert(0, Span::styled(prefix.clone(), prefix_style));
            }
            self.out.push(line);
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        // Trim a trailing blank introduced by the last block separator.
        while self.out.last().is_some_and(|l| line_is_empty(l)) {
            self.out.pop();
        }
        self.out
    }
}

/// Render one table row (wrapping cells to their columns) into `out`. No block
/// prefix is applied here; the caller prepends it uniformly to every line.
fn push_table_row(
    cells: &[String],
    widths: &[usize],
    alignments: &[Alignment],
    cell_style: Style,
    border: Style,
    out: &mut Vec<Line<'static>>,
) {
    // Wrap each cell to its column width; the row is as tall as its tallest cell.
    let wrapped: Vec<Vec<String>> = widths
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let text = cells.get(i).map(String::as_str).unwrap_or("");
            wrap_plain(text, *w)
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let bar = Span::styled("\u{2502}".to_string(), border);
    for line_idx in 0..height {
        let mut spans = vec![bar.clone(), Span::raw(" ")];
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" "));
                spans.push(bar.clone());
                spans.push(Span::raw(" "));
            }
            let empty = String::new();
            let cell = wrapped[i].get(line_idx).unwrap_or(&empty);
            // Defensive clamp: a glyph wider than a 1-column slot cannot be
            // split, so guarantee the rendered cell never exceeds its column
            // and pushes the border out of alignment.
            let text = truncate_to_width(cell, *w);
            let align = alignments.get(i).copied().unwrap_or(Alignment::None);
            let (left, right) = pad_for(UnicodeWidthStr::width(text.as_str()), *w, align);
            if left > 0 {
                spans.push(Span::raw(" ".repeat(left)));
            }
            spans.push(Span::styled(text, cell_style));
            if right > 0 {
                spans.push(Span::raw(" ".repeat(right)));
            }
        }
        spans.push(Span::raw(" "));
        spans.push(bar.clone());
        out.push(Line::from(spans));
    }
}

/// Fallback for tables too narrow for a stable box: pipe-joined rows wrapped to
/// `width` so even the fallback never overflows the available columns.
fn render_table_fallback(
    table: &TableState,
    body_style: Style,
    header_style: Style,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let mut push_row = |cells: &[String], style: Style| {
        let joined = cells.join(" | ");
        for line in wrap_plain(&joined, width) {
            out.push(Line::from(Span::styled(line, style)));
        }
    };
    push_row(&table.header, header_style);
    for row in &table.rows {
        push_row(row, body_style);
    }
}

/// Distribute `available` columns across `natural` widths: keep natural widths
/// when they fit, otherwise shrink proportionally with a floor of 1 per column.
fn fit_columns(natural: &[usize], available: usize) -> Vec<usize> {
    let total: usize = natural.iter().fold(0usize, |acc, w| acc.saturating_add(*w));
    if total <= available {
        return natural.iter().map(|w| (*w).max(1)).collect();
    }
    let cols = natural.len();
    // Proportional shrink with a minimum of 1 column each.
    let mut widths: Vec<usize> = natural
        .iter()
        .map(|w| {
            let scaled = (*w as f64) * (available as f64) / (total as f64);
            (scaled.floor() as usize).max(1)
        })
        .collect();
    // Fix rounding drift so the columns exactly fill `available`.
    let mut sum: usize = widths.iter().sum();
    let mut i = 0;
    while sum > available && i < cols * 4 {
        let idx = i % cols;
        if widths[idx] > 1 {
            widths[idx] -= 1;
            sum -= 1;
        }
        i += 1;
    }
    i = 0;
    while sum < available && i < cols * 4 {
        let idx = i % cols;
        widths[idx] += 1;
        sum += 1;
        i += 1;
    }
    widths
}

/// Truncate `text` to at most `max` display columns on a char boundary, so a
/// rendered table cell never overflows its column (e.g. a wide glyph that cannot
/// be split into a 1-column slot).
fn truncate_to_width(text: &str, max: usize) -> String {
    if UnicodeWidthStr::width(text) <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if used + w > max {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// Left/right padding to fit `text_width` into `col` for the given alignment.
fn pad_for(text_width: usize, col: usize, align: Alignment) -> (usize, usize) {
    let slack = col.saturating_sub(text_width);
    match align {
        Alignment::Right => (slack, 0),
        Alignment::Center => (slack / 2, slack - slack / 2),
        Alignment::None | Alignment::Left => (0, slack),
    }
}

/// Wrap `text` to `width` columns on word boundaries, hard-breaking words that
/// are themselves wider than the column. Always returns at least one line.
fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for word in text.split_whitespace() {
        let word_w = UnicodeWidthStr::width(word);
        if word_w > width {
            // Flush, then hard-break the oversized word by display column.
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_w = 0;
            }
            let mut chunk = String::new();
            let mut chunk_w = 0;
            for ch in word.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
                if chunk_w + cw > width {
                    lines.push(std::mem::take(&mut chunk));
                    chunk_w = 0;
                }
                chunk.push(ch);
                chunk_w += cw;
            }
            if !chunk.is_empty() {
                current = chunk;
                current_w = chunk_w;
            }
            continue;
        }
        let sep = usize::from(!current.is_empty());
        if current_w + sep + word_w > width {
            lines.push(std::mem::take(&mut current));
            current_w = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_w += 1;
        }
        current.push_str(word);
        current_w += word_w;
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn line_is_empty(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn rendered(md: &str) -> Vec<String> {
        render_markdown(md).iter().map(text_of).collect()
    }

    #[test]
    fn heading_is_bold_text_without_hashes() {
        let lines = render_markdown("# Title");
        let title = lines.iter().find(|l| text_of(l) == "Title").expect("title");
        assert!(
            title
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "heading not bold"
        );
        assert!(!text_of(title).contains('#'));
    }

    #[test]
    fn bold_and_italic_spans_are_styled() {
        let lines = render_markdown("a **b** _c_");
        let line = &lines[0];
        let bold = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "b")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let italic = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "c")
            .expect("italic span");
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn inline_html_text_is_preserved_not_dropped() {
        // Angle-bracket content (raw/inline HTML) common in model output must
        // survive verbatim instead of vanishing.
        let lines = render_markdown("before <thinking>step</thinking> after");
        let joined: String = lines.iter().map(text_of).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("<thinking>"),
            "opening tag dropped: {joined:?}"
        );
        assert!(joined.contains("step"), "tag content dropped: {joined:?}");
        assert!(
            joined.contains("</thinking>"),
            "closing tag dropped: {joined:?}"
        );
    }

    #[test]
    fn inline_code_keeps_backticks_and_distinct_style() {
        let lines = render_markdown("run `cargo test` now");
        let code = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "`cargo test`")
            .expect("inline code span");
        assert_eq!(code.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn fenced_code_block_is_dim_and_indented_without_highlighting() {
        let md = "```\nlet x = 1;\nlet y = 2;\n```";
        let lines = render_markdown(md);
        let code: Vec<&Line> = lines
            .iter()
            .filter(|l| text_of(l).contains("let "))
            .collect();
        assert_eq!(code.len(), 2);
        for line in code {
            assert!(text_of(line).starts_with("    "), "code not indented");
            assert!(
                line.spans
                    .iter()
                    .all(|s| s.style.add_modifier.contains(Modifier::DIM)),
                "code not dim / was highlighted"
            );
        }
    }

    #[test]
    fn fenced_code_block_inside_blockquote_keeps_quote_prefix() {
        let out = rendered("> ```\n> code\n> ```");
        assert!(
            out.iter().any(|l| l == ">     code"),
            "blockquote code lost prefix: {out:?}"
        );
    }

    #[test]
    fn fenced_code_block_inside_list_stays_indented_under_item() {
        let out = rendered("- item\n\n  ```\n  code\n  ```");
        assert!(out.iter().any(|l| l == "- item"));
        assert!(
            out.iter().any(|l| l == "      code"),
            "list code lost indent: {out:?}"
        );
    }

    #[test]
    fn list_item_second_paragraph_keeps_continuation_indent() {
        let out = rendered("- first\n\n  second");
        assert!(out.iter().any(|l| l == "- first"));
        assert!(
            out.iter().any(|l| l == "  second"),
            "continuation lost indent: {out:?}"
        );
    }

    #[test]
    fn parent_list_item_continuation_after_nested_list_keeps_parent_indent() {
        let out = rendered("- parent\n\n  - child\n\n  continuation");
        assert!(out.iter().any(|l| l == "- parent"));
        assert!(out.iter().any(|l| l == "  - child"));
        assert!(
            out.iter().any(|l| l == "  continuation"),
            "parent continuation lost indent: {out:?}"
        );
    }

    #[test]
    fn bullet_list_items_get_dash_markers() {
        let out = rendered("- one\n- two");
        assert!(out.iter().any(|l| l == "- one"));
        assert!(out.iter().any(|l| l == "- two"));
    }

    #[test]
    fn ordered_list_numbers_increment() {
        let out = rendered("1. first\n2. second");
        assert!(out.iter().any(|l| l == "1. first"));
        assert!(out.iter().any(|l| l == "2. second"));
    }

    #[test]
    fn nested_list_is_indented() {
        let out = rendered("- top\n  - nested");
        assert!(out.iter().any(|l| l == "- top"));
        assert!(out.iter().any(|l| l == "  - nested"));
    }

    #[test]
    fn blockquote_is_prefixed_and_dim() {
        let lines = render_markdown("> quoted text");
        let quote = lines
            .iter()
            .find(|l| text_of(l).contains("quoted text"))
            .expect("quote line");
        assert!(text_of(quote).starts_with("> "));
        let body = quote
            .spans
            .iter()
            .find(|s| s.content.as_ref().contains("quoted"))
            .expect("quote body");
        assert!(body.style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn blockquoted_list_does_not_leave_trailing_prefix_line() {
        assert_eq!(rendered("> - item"), vec!["> - item".to_string()]);
    }

    #[test]
    fn paragraphs_separated_by_blank_line() {
        let out = rendered("para one\n\npara two");
        let blank = out.iter().position(|l| l.is_empty());
        assert!(blank.is_some(), "no blank between paragraphs: {out:?}");
        assert!(out.first().is_some_and(|l| !l.is_empty()), "leading blank");
        assert!(out.last().is_some_and(|l| !l.is_empty()), "trailing blank");
    }

    #[test]
    fn plain_text_round_trips_as_one_line() {
        assert_eq!(
            rendered("just a sentence"),
            vec!["just a sentence".to_string()]
        );
    }

    #[test]
    fn inline_link_keeps_visible_destination() {
        assert_eq!(
            rendered("Read [the guide](https://example.com/docs)."),
            vec!["Read the guide (https://example.com/docs).".to_string()]
        );
    }

    #[test]
    fn link_destination_is_not_duplicated_when_label_is_destination() {
        assert_eq!(
            rendered("Visit <https://example.com/docs>"),
            vec!["Visit https://example.com/docs".to_string()]
        );
    }

    #[test]
    fn reference_link_keeps_resolved_destination() {
        assert_eq!(
            rendered("See [spec][s].\n\n[s]: https://example.com/spec"),
            vec!["See spec (https://example.com/spec).".to_string()]
        );
    }

    // ---- New features ----

    #[test]
    fn strikethrough_text_is_crossed_out() {
        let lines = render_markdown("a ~~gone~~ b");
        let span = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "gone")
            .expect("strikethrough span");
        assert!(
            span.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "not crossed out: {:?}",
            span.style
        );
    }

    #[test]
    fn task_list_renders_checkboxes() {
        let out = rendered("- [ ] todo\n- [x] done");
        assert!(
            out.iter().any(|l| l == "- [ ] todo"),
            "unchecked box missing: {out:?}"
        );
        assert!(
            out.iter().any(|l| l == "- [x] done"),
            "checked box missing: {out:?}"
        );
    }

    #[test]
    fn table_renders_aligned_box() {
        let md = "| A | B |\n| - | - |\n| 1 | 2 |";
        let out = rendered(md);
        assert!(
            out.iter()
                .any(|l| l.starts_with('\u{250c}') && l.contains('\u{252c}')),
            "no top border: {out:?}"
        );
        assert!(
            out.iter()
                .any(|l| l.contains("A") && l.contains('\u{2502}')),
            "header row missing: {out:?}"
        );
        assert!(
            out.iter().any(|l| l.contains("1") && l.contains("2")),
            "body row missing: {out:?}"
        );
        assert!(
            out.iter().any(|l| l.starts_with('\u{2514}')),
            "no bottom border: {out:?}"
        );
    }

    #[test]
    fn table_header_is_bold() {
        let md = "| Name | Val |\n| - | - |\n| x | 1 |";
        let lines = render_markdown(md);
        let header = lines
            .iter()
            .find(|l| text_of(l).contains("Name"))
            .expect("header line");
        assert!(
            header.spans.iter().any(
                |s| s.content.contains("Name") && s.style.add_modifier.contains(Modifier::BOLD)
            ),
            "header not bold: {header:?}"
        );
    }

    #[test]
    fn table_lines_fit_render_width() {
        // Long cells must wrap so every physical table line fits the width and
        // the downstream wrapper never breaks the box.
        let md = "| Column one heading | Column two heading |\n| - | - |\n| a fairly long cell value here | another long value goes here |";
        let width = 40;
        let lines = render_markdown_themed(md, &MarkdownTheme::default(), width);
        for line in &lines {
            assert!(
                UnicodeWidthStr::width(text_of(line).as_str()) <= width,
                "table line exceeds width {width}: {:?}",
                text_of(line)
            );
        }
    }

    #[test]
    fn blockquoted_table_keeps_quote_prefix() {
        let md = "> | A | B |\n> | - | - |\n> | 1 | 2 |";
        let out = rendered(md);
        assert!(
            out.iter().all(|l| l.starts_with("> ")),
            "every blockquoted table line must keep the quote prefix: {out:?}"
        );
        assert!(
            out.iter().any(|l| l.contains('\u{250c}')),
            "no box: {out:?}"
        );
    }

    #[test]
    fn nested_list_table_keeps_indent_and_fits_width() {
        let md = "- item\n\n  | A | B |\n  | - | - |\n  | 1 | 2 |";
        let width = 40;
        let lines = render_markdown_themed(md, &MarkdownTheme::default(), width);
        let table_lines: Vec<String> = lines
            .iter()
            .map(text_of)
            .filter(|l| l.contains('\u{2502}') || l.contains('\u{250c}') || l.contains('\u{2514}'))
            .collect();
        assert!(!table_lines.is_empty(), "no table rendered");
        for l in &table_lines {
            assert!(l.starts_with("  "), "table line lost list indent: {l:?}");
            assert!(
                UnicodeWidthStr::width(l.as_str()) <= width,
                "indented table line exceeds width: {l:?}"
            );
        }
    }

    #[test]
    fn narrow_table_falls_back_to_plain_rows() {
        let md = "| AAAA | BBBB | CCCC |\n| - | - | - |\n| 1 | 2 | 3 |";
        // Too narrow for a stable box (cols * overhead > width).
        let lines = render_markdown_themed(md, &MarkdownTheme::default(), 6);
        let joined: String = lines.iter().map(text_of).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("AAAA") && joined.contains("|"),
            "fallback row missing: {joined:?}"
        );
        // No box border characters in the fallback.
        assert!(
            !joined.contains('\u{250c}'),
            "narrow table should not draw a box: {joined:?}"
        );
    }

    #[test]
    fn theme_base_style_applies_to_all_spans() {
        let theme = MarkdownTheme::thinking();
        let lines = render_markdown_themed("plain **bold** text", &theme, DEFAULT_RENDER_WIDTH);
        for span in &lines[0].spans {
            assert!(
                span.style.add_modifier.contains(Modifier::ITALIC)
                    && span.style.add_modifier.contains(Modifier::DIM),
                "base dim+italic not applied to span {span:?}"
            );
        }
    }

    #[test]
    fn link_theme_seam_styles_label() {
        let theme = MarkdownTheme {
            link: Style::default().fg(Color::Blue),
            ..MarkdownTheme::default()
        };
        let lines =
            render_markdown_themed("see [docs](https://x.dev)", &theme, DEFAULT_RENDER_WIDTH);
        let label = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "docs")
            .expect("link label span");
        assert_eq!(label.style.fg, Some(Color::Blue));
    }

    #[test]
    fn highlight_hook_is_used_for_fenced_blocks() {
        let theme = MarkdownTheme {
            highlight_code: Some(Rc::new(|code: &str, lang: Option<&str>| {
                vec![Line::from(Span::raw(format!(
                    "HL[{}]:{}",
                    lang.unwrap_or("none"),
                    code.replace('\n', "/")
                )))]
            })),
            ..MarkdownTheme::default()
        };
        let md = "```rust\nlet x = 1;\n```";
        let lines = render_markdown_themed(md, &theme, DEFAULT_RENDER_WIDTH);
        let joined: String = lines.iter().map(text_of).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("HL[rust]:let x = 1;"),
            "highlight hook not honored: {joined:?}"
        );
    }

    #[test]
    fn default_theme_matches_legacy_inline_code_color() {
        // Guards the theme refactor against drift from historical hardcoded styles.
        let lines = render_markdown("`x`");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Cyan));
    }
}
