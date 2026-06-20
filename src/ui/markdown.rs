//! Minimal Markdown -> ratatui `Line` mapper for assistant text (Tier 3).
//!
//! Iris renders streamed/finalized assistant messages as Markdown so headings,
//! emphasis, code, lists, and quotes read the way the model intends. This is a
//! deliberately small mapper over `pulldown-cmark`'s event stream -- NOT a full
//! renderer. It covers headings, bold, italic, inline code, fenced code blocks
//! (dimmed + indented, no syntax highlighting), bullet/ordered lists, and
//! blockquotes. Tables, links/OSC-8 hyperlinks, images, and syntax highlighting
//! are intentionally out of scope (see ROADMAP / the TUI re-architecture spec).
//!
//! Output is `Vec<Line<'static>>`; the caller wraps each line to the transcript
//! width (so this layer is width-agnostic and stays testable without a frame).

use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Indent applied per Markdown nesting level (list depth / blockquote).
const INDENT: &str = "  ";

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn inline_code_style() -> Style {
    Style::default().fg(Color::Cyan)
}

/// Render Markdown `text` into styled transcript lines. Returns at least one
/// (possibly empty) line for non-empty input so an assistant message never
/// vanishes; empty input yields no lines.
pub(crate) fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let mut renderer = Renderer::default();
    for event in Parser::new(text) {
        renderer.event(event);
    }
    renderer.finish()
}

#[derive(Default)]
struct Renderer {
    out: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    /// Ordered-list counters (`Some(n)`) or bullet markers (`None`), one per
    /// open list, so nesting indents and numbering stay correct.
    lists: Vec<Option<u64>>,
    bold: u32,
    italic: u32,
    quote: u32,
    heading: bool,
    in_code_block: bool,
    /// Prefix (indent + marker / quote markers) for the line being built.
    prefix: String,
}

impl Renderer {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.push_span(format!("`{code}`"), inline_code_style()),
            Event::SoftBreak => self.push_span(" ".to_string(), self.text_style()),
            Event::HardBreak => self.flush(),
            // Horizontal rule: a dim divider line.
            Event::Rule => {
                self.flush();
                self.out.push(Line::from(Span::styled("---", dim())));
            }
            // Raw/inline HTML (and angle-bracket text like `<thinking>` that
            // model output often contains): render the literal text rather than
            // dropping it, so nothing silently vanishes from the transcript.
            Event::Html(html) | Event::InlineHtml(html) => self.text(&html),
            // Footnotes, math, task markers: nothing to render here.
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { .. } => self.heading = true,
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::BlockQuote(_) => self.quote += 1,
            Tag::CodeBlock(_) => {
                self.flush();
                self.in_code_block = true;
            }
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
            TagEnd::BlockQuote(_) => self.quote = self.quote.saturating_sub(1),
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                self.blank();
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.prefix = INDENT.repeat(self.lists.len());
                self.blank();
            }
            TagEnd::Item => {
                self.flush();
                self.prefix.clear();
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if self.in_code_block {
            // Each source line of a fenced block is its own dim, indented row;
            // no syntax highlighting. `lines()` drops the trailing newline.
            let prefix = self.code_block_prefix();
            for line in text.split('\n') {
                // Skip the final empty segment from a trailing newline.
                if line.is_empty() && text.ends_with('\n') {
                    continue;
                }
                self.out
                    .push(Line::from(Span::styled(format!("{prefix}{line}"), dim())));
            }
            return;
        }
        let style = self.text_style();
        self.push_span(text.to_string(), style);
    }

    fn text_style(&self) -> Style {
        let mut style = Style::default();
        if self.bold > 0 || self.heading {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.quote > 0 {
            style = style.fg(Color::DarkGray);
        }
        style
    }

    fn push_span(&mut self, content: String, style: Style) {
        if content.is_empty() {
            return;
        }
        self.spans.push(Span::styled(content, style));
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
    /// still emitted so structure is visible.
    fn flush(&mut self) {
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
            line.push(Span::styled(prefix, dim()));
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

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        // Trim a trailing blank introduced by the last block separator.
        while self.out.last().is_some_and(|l| line_is_empty(l)) {
            self.out.pop();
        }
        self.out
    }
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
                    .all(|s| s.style.fg == Some(Color::DarkGray)),
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
        assert_eq!(body.style.fg, Some(Color::DarkGray));
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
}
