//! Syntax highlighter for fenced Markdown code blocks (Tier 3).
//!
//! Implements the `HighlightFn` seam in [`crate::ui::markdown`] (#324, boundary
//! ADR-0033): a fenced block's language tag is mapped to a `syntect` syntax, the
//! code is parsed into scoped tokens, and each token's scope is mapped to a
//! fixed slice of the Iris design-system palette ([`crate::ui::palette`]). This
//! is a deliberately small scope->color mapping, not a stock theme dump: it
//! covers keywords, strings, comments, types, functions, and literals, and
//! emits no background colors (the transcript surface owns the background).
//!
//! # Why an in-repo mapper instead of `syntect-tui`
//!
//! Per the reuse ladder, we considered `syntect-tui` for the syntect->ratatui
//! `Style` mapping and rejected it: it is a low-maturity, low-traffic crate not
//! verified against ratatui 0.30, and it exists to translate a full syntect
//! `Theme` into `ratatui::Style`. We never build a syntect `Theme` (we map raw
//! scopes to the palette directly), so its surface does not fit; the mapping we
//! do need is ~1 match arm and trivial to own. syntect itself is the load-bearing
//! dependency (regex-driven syntax parsing); the style glue is not worth a dep.
//!
//! The syntax set is loaded lazily once via a `OnceLock` (syntax dumps are ~1MB
//! to deserialize) and reused for every subsequent block; the one-time load time
//! is measured and emitted at `tracing::debug`.

use std::rc::Rc;
use std::sync::OnceLock;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::ScopeRegionIterator;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::ui::markdown::HighlightFn;
use crate::ui::palette;

/// Lazily-loaded default syntax set (newline mode, to pair with
/// [`LinesWithEndings`]). The bundled Sublime syntax dump is ~1MB to
/// deserialize, so we load it once on first use and reuse it thereafter;
/// the load time is measured and logged at `debug`.
fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(|| {
        let start = std::time::Instant::now();
        let set = SyntaxSet::load_defaults_newlines();
        tracing::debug!(
            elapsed_ms = start.elapsed().as_secs_f64() * 1000.0,
            syntaxes = set.syntaxes().len(),
            "loaded syntect syntax set"
        );
        set
    })
}

/// Highlight `code` for language tag `lang`, returning one styled [`Line`] per
/// source line. Returns `None` when `lang` is absent or does not resolve to a
/// known syntax, so the caller falls back to today's dim rendering. Spans carry
/// only a foreground color (no background); the caller composes the theme base
/// (e.g. the thinking dim+italic) on top.
pub(crate) fn highlight(code: &str, lang: Option<&str>) -> Option<Vec<Line<'static>>> {
    // Fence info strings may carry attributes after the language (e.g.
    // "rust ignore", "python title=x"); only the first token names the syntax.
    let lang = lang?.split_whitespace().next()?;
    let syntax_set = syntax_set();
    // `find_syntax_by_token` matches names, aliases, and file extensions
    // (e.g. "rust"/"rs", "python"/"py"), so it covers the tags models emit.
    let syntax = syntax_set.find_syntax_by_token(lang)?;

    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut lines: Vec<Line<'static>> = Vec::new();

    for line in LinesWithEndings::from(code) {
        // A malformed/oversized line should degrade to the dim fallback rather
        // than render a partial block, so bail the whole block on parse error.
        let ops = state.parse_line(line, syntax_set).ok()?;
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (text, op) in ScopeRegionIterator::new(&ops, line) {
            stack.apply(op).ok()?;
            // The last region of each line carries the trailing '\n' (newline
            // mode); strip it so it never enters a `Line`.
            let text = text.strip_suffix('\n').unwrap_or(text);
            if text.is_empty() {
                continue;
            }
            let style = scope_style(&stack);
            spans.push(Span::styled(text.to_string(), style));
        }
        lines.push(Line::from(spans));
    }

    Some(lines)
}

/// Return whether `token` names a bundled syntax. This is used by tool-panel
/// inference before opting an otherwise opaque output stream into highlighting.
pub(crate) fn is_known_syntax(token: &str) -> bool {
    syntax_set().find_syntax_by_token(token).is_some()
}

/// Build the `HighlightFn` the Markdown renderer injects at the fenced-code
/// seam. `base` is the theme's base style (e.g. the thinking dim+italic) and
/// `fallback` is the dim code-block style used when a language is unknown.
///
/// On a hit, the palette color is patched *onto* `base` so the thinking variant
/// stays dimmed and the normal variant renders the raw color. On a miss, the
/// block is emitted exactly as today's dim path: one span per line at
/// `fallback`, split on `\n` with no trailing empty line (the renderer strips
/// the block's final newline before calling us).
pub(crate) fn code_highlighter(base: Style, fallback: Style) -> HighlightFn {
    Rc::new(
        move |code: &str, lang: Option<&str>| match highlight(code, lang) {
            Some(lines) => lines
                .into_iter()
                .map(|line| {
                    let spans = line
                        .spans
                        .into_iter()
                        .map(|span| Span::styled(span.content, base.patch(span.style)))
                        .collect::<Vec<_>>();
                    Line::from(spans)
                })
                .collect(),
            None => code
                .split('\n')
                .map(|l| Line::from(Span::styled(l.to_string(), fallback)))
                .collect(),
        },
    )
}

/// Map a token's scope stack to a palette foreground color. The most specific
/// (top-of-stack) scope wins; unrecognized scopes get the default style so
/// punctuation and identifiers read as plain foreground text. Colors bind to the
/// ANSI-named palette roles so highlighting inherits the user's terminal theme.
fn scope_style(stack: &ScopeStack) -> Style {
    for scope in stack.as_slice().iter().rev() {
        if let Some(style) = scope_name_style(*scope) {
            return style;
        }
    }
    Style::default()
}

/// The scope-atom prefixes the mapper recognizes, parsed once. Prefix matching
/// via [`Scope::is_prefix_of`] compares packed atom ids, so the per-token hot
/// loop in [`scope_style`] allocates nothing (`build_string` allocated a
/// `String` per scope per token).
struct ScopePrefixes {
    comment: Scope,
    string: Scope,
    constant_character: Scope,
    constant: Scope,
    keyword: Scope,
    storage: Scope,
    entity_name_function: Scope,
    support_function: Scope,
    variable_function: Scope,
    entity_name: Scope,
    support_type: Scope,
    support_class: Scope,
}

fn scope_prefixes() -> &'static ScopePrefixes {
    static PREFIXES: OnceLock<ScopePrefixes> = OnceLock::new();
    PREFIXES.get_or_init(|| {
        let s = |name: &str| Scope::new(name).expect("static scope atom");
        ScopePrefixes {
            comment: s("comment"),
            string: s("string"),
            constant_character: s("constant.character"),
            constant: s("constant"),
            keyword: s("keyword"),
            storage: s("storage"),
            entity_name_function: s("entity.name.function"),
            support_function: s("support.function"),
            variable_function: s("variable.function"),
            entity_name: s("entity.name"),
            support_type: s("support.type"),
            support_class: s("support.class"),
        }
    })
}

/// Fixed scope-atom -> palette mapping. Keyed on the leading dotted atoms of a
/// TextMate/Sublime scope name. Intentionally coarse: a handful of roles reads as
/// a muted IDE theme without chasing full theme fidelity.
fn scope_name_style(scope: Scope) -> Option<Style> {
    let p = scope_prefixes();
    // Order matters: check the more specific `constant`/`entity` prefixes before
    // falling through to their broader atoms.
    if p.comment.is_prefix_of(scope) {
        // Comments are the one muted role: gray + dim so code reads first.
        return Some(
            Style::default()
                .fg(palette::border())
                .add_modifier(Modifier::DIM),
        );
    }
    if p.string.is_prefix_of(scope) || p.constant_character.is_prefix_of(scope) {
        return Some(Style::default().fg(palette::green()));
    }
    if p.constant.is_prefix_of(scope) {
        // Numeric / language literals (numbers, true/false/nil).
        return Some(Style::default().fg(palette::red()));
    }
    if p.keyword.is_prefix_of(scope) || p.storage.is_prefix_of(scope) {
        // Keywords and storage keywords (`fn`, `let`, `struct`, `class`).
        return Some(Style::default().fg(palette::orange()));
    }
    if p.entity_name_function.is_prefix_of(scope)
        || p.support_function.is_prefix_of(scope)
        || p.variable_function.is_prefix_of(scope)
    {
        return Some(Style::default().fg(palette::cyan()));
    }
    if p.entity_name.is_prefix_of(scope)
        || p.support_type.is_prefix_of(scope)
        || p.support_class.is_prefix_of(scope)
    {
        // Type / class / trait names.
        return Some(Style::default().fg(palette::cyan()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_info_attributes_after_language_still_resolve() {
        // Models emit fences like ```rust ignore or ```python title=x; only the
        // first token names the syntax.
        let lines = highlight("let x = 1;\n", Some("rust ignore")).expect("resolves");
        assert!(distinct_colors(&lines).len() > 1, "expected styled spans");
        assert!(
            highlight("code\n", Some("  ")).is_none(),
            "blank info -> fallback"
        );
    }

    fn distinct_colors(
        lines: &[Line<'static>],
    ) -> std::collections::HashSet<Option<ratatui::style::Color>> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.style.fg)
            .collect()
    }

    #[test]
    fn known_language_produces_multiple_styles() {
        let code = "fn main() {\n    let x = 42; // note\n}";
        let lines = highlight(code, Some("rust")).expect("rust is a known syntax");
        assert_eq!(lines.len(), 3, "one styled line per source line");
        // A keyword, a number, a comment, and plain text must not collapse to a
        // single color: an IDE-grade block has several distinct roles.
        assert!(
            distinct_colors(&lines).len() > 1,
            "expected multiple distinct span colors, got {:?}",
            distinct_colors(&lines)
        );
    }

    #[test]
    fn language_alias_and_extension_resolve() {
        // `find_syntax_by_token` must resolve both the name and the extension.
        assert!(highlight("x = 1\n", Some("python")).is_some());
        assert!(highlight("x = 1\n", Some("py")).is_some());
        assert!(highlight("let x = 1;\n", Some("rs")).is_some());
    }

    #[test]
    fn unknown_language_returns_none() {
        assert!(highlight("some text", Some("definitely-not-a-language")).is_none());
    }

    #[test]
    fn absent_language_returns_none() {
        assert!(highlight("plain code", None).is_none());
    }

    #[test]
    fn empty_code_is_some_and_empty() {
        // A known language with no body must not panic and yields no lines.
        let lines = highlight("", Some("rust")).expect("known syntax");
        assert!(lines.is_empty(), "empty code should produce no lines");
    }

    #[test]
    fn comment_is_dim_gray() {
        let lines = highlight("// hello\n", Some("rust")).expect("rust");
        let comment = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("hello"))
            .expect("comment span");
        assert_eq!(comment.style.fg, Some(palette::border()));
        assert!(comment.style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn code_highlighter_dims_hit_spans_for_thinking_base() {
        // The thinking variant patches its dim+italic base onto every colored
        // span so highlighted code stays visually muted.
        let base = Style::default()
            .add_modifier(Modifier::DIM)
            .add_modifier(Modifier::ITALIC);
        let hl = code_highlighter(base, base);
        let lines = hl("let x = 1;", Some("rust"));
        assert!(!lines.is_empty());
        for span in lines.iter().flat_map(|l| l.spans.iter()) {
            assert!(
                span.style.add_modifier.contains(Modifier::DIM)
                    && span.style.add_modifier.contains(Modifier::ITALIC),
                "thinking base not composed onto highlighted span: {span:?}"
            );
        }
    }

    #[test]
    fn code_highlighter_falls_back_to_dim_for_unknown_language() {
        // Unknown language must render exactly as today: one span per line at
        // the fallback (dim) style, no trailing empty line.
        let fallback = Style::default().add_modifier(Modifier::DIM);
        let hl = code_highlighter(Style::default(), fallback);
        let lines = hl("line one\nline two", Some("no-such-lang"));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].content.as_ref(), "line one");
        assert_eq!(lines[0].spans[0].style, fallback);
        assert_eq!(lines[1].spans[0].content.as_ref(), "line two");
    }

    #[test]
    fn lazy_syntax_set_reuses_same_instance() {
        // OnceLock semantics: first call initializes, second returns the same
        // pointer (no reload).
        let first = syntax_set() as *const SyntaxSet;
        let second = syntax_set() as *const SyntaxSet;
        assert_eq!(first, second, "syntax set must be loaded once and reused");
    }

    #[test]
    fn wide_glyph_code_is_preserved_and_width_measurable() {
        // A CJK glyph inside highlighted code must survive as an intact cluster;
        // the wrap-safety width test lives in the wrap module where the
        // span-aware wrapper is in scope.
        let code = "let s = \"\u{4e2d}\u{6587}\";\n";
        let lines = highlight(code, Some("rust")).expect("rust");
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains('\u{4e2d}') && joined.contains('\u{6587}'));
    }
}
