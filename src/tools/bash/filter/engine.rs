//! Declarative output-filter pipeline (ADR-0037).
//!
//! A filter is a data-driven pipeline of eight ordered stages, defined in TOML
//! (design and schema ported from RTK, rtk-ai/rtk, Apache-2.0 -- see
//! `data/NOTICE.md`):
//!
//!   1. `strip_ansi`         -- remove ANSI escape codes
//!   2. `replace`            -- regex substitutions, line-by-line, chainable
//!   3. `match_output`       -- short-circuit: whole-blob match returns a fixed
//!      message; every rule carries an `unless` error-guard and the stage is
//!      skipped entirely when the command did not exit 0
//!   4. `strip`/`keep` lines -- filter lines by regex; lines matching the
//!      global error-guard are always retained (never stripped, always kept)
//!   5. `truncate_lines_at`  -- truncate each line to N chars
//!   6. `head`/`tail` lines  -- keep first/last N lines with an omit marker
//!   7. `max_lines`          -- absolute line cap with a truncation marker
//!   8. `on_empty`           -- fixed message when the result is empty
//!
//! Quality guards baked into the engine (not left to filter authors):
//! - short-circuit success messages are disabled on non-zero exit;
//! - the error-guard regex exempts error/failure lines from stage 4;
//! - the lossy size-reduction stages (5-7) and the success-flavored `on_empty`
//!   message (8) are disabled on non-zero exit, so a failed command's
//!   diagnostics survive verbatim (ADR-0036 "failure is complete"); the bash
//!   tool's `truncate_tail` stays the final safety backstop;
//! - compile errors in a definition drop that filter (never a panic).

use std::collections::BTreeMap;

use regex::{Regex, RegexSet};
use serde::Deserialize;

/// Lines matching this are never removed by the strip/keep stage, regardless
/// of what a filter definition asks for. The pattern is deliberately precise:
/// it targets error/failure *signals* (compiler `error:`/`error[`, test
/// `FAILED`, panics, `fatal:`, tracebacks), not any line containing the word
/// "error" -- summary chatter like "1 error generated." stays strippable when
/// the errors themselves are kept.
const ERROR_GUARD_PATTERN: &str = concat!(
    r"error(:|\[)|Error:|\bERROR\b|\bFAILED\b|\bFAIL\b|\bfailures?:",
    r"|panicked at|panic:|\bfatal(:| error)|Traceback \(most recent call",
    r"|Segmentation fault|assertion .*failed|✗|✕"
);

fn error_guard() -> &'static Regex {
    static GUARD: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    GUARD.get_or_init(|| Regex::new(ERROR_GUARD_PATTERN).expect("error-guard pattern is static"))
}

fn ansi_re() -> &'static Regex {
    static ANSI: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").expect("ANSI pattern is static"))
}

/// Strip ANSI escape sequences (colors, styles) from a line.
pub(super) fn strip_ansi(text: &str) -> String {
    ansi_re().replace_all(text, "").into_owned()
}

// ---------------------------------------------------------------------------
// TOML schema (deserialization types)
// ---------------------------------------------------------------------------

/// Short-circuit rule: if `pattern` matches the whole output blob, return
/// `message` instead of the output. `unless` is a mandatory error-guard: when
/// it also matches, the rule is skipped so a success summary can never mask an
/// error. (RTK makes `unless` optional; Iris requires it -- ADR-0037.)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchOutputRule {
    pattern: String,
    message: String,
    unless: String,
}

/// A regex substitution applied line-by-line; rules chain sequentially.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceRule {
    pattern: String,
    replacement: String,
}

/// Inline test case attached to a filter (`[[tests.<name>]]`). Consumed only
/// by the unit-test runner in `mod.rs`; the release build parses and ignores
/// the sections.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct FilterTestDef {
    pub(super) name: String,
    pub(super) input: String,
    pub(super) expected: String,
}

impl FilterFile {
    /// Number of filter definitions in the file (compiled or not).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn filter_count(&self) -> usize {
        self.filters.len()
    }
}

#[derive(Deserialize)]
pub(super) struct FilterFile {
    schema_version: u32,
    #[serde(default)]
    filters: BTreeMap<String, FilterDef>,
    /// Inline tests keyed by filter name; separate from `filters` so the
    /// filter defs keep `deny_unknown_fields`.
    #[serde(default)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) tests: BTreeMap<String, Vec<FilterTestDef>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FilterDef {
    #[allow(dead_code)]
    description: Option<String>,
    match_command: String,
    #[serde(default)]
    strip_ansi: bool,
    #[serde(default)]
    replace: Vec<ReplaceRule>,
    #[serde(default)]
    match_output: Vec<MatchOutputRule>,
    #[serde(default)]
    strip_lines_matching: Vec<String>,
    #[serde(default)]
    keep_lines_matching: Vec<String>,
    truncate_lines_at: Option<usize>,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    max_lines: Option<usize>,
    on_empty: Option<String>,
    /// Accepted for RTK data-file compatibility; Iris always filters the
    /// merged stdout+stderr stream, so this is a no-op here.
    #[serde(default)]
    #[allow(dead_code)]
    filter_stderr: bool,
}

// ---------------------------------------------------------------------------
// Compiled types
// ---------------------------------------------------------------------------

struct CompiledMatchOutputRule {
    pattern: Regex,
    message: String,
    unless: Regex,
}

struct CompiledReplaceRule {
    pattern: Regex,
    replacement: String,
}

enum LineFilter {
    None,
    Strip(RegexSet),
    Keep(RegexSet),
}

/// A filter whose regexes are compiled and ready to apply.
pub(super) struct CompiledFilter {
    pub(super) name: String,
    match_regex: Regex,
    strip_ansi: bool,
    replace: Vec<CompiledReplaceRule>,
    match_output: Vec<CompiledMatchOutputRule>,
    line_filter: LineFilter,
    truncate_lines_at: Option<usize>,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    max_lines: Option<usize>,
    on_empty: Option<String>,
}

impl CompiledFilter {
    pub(super) fn matches(&self, command: &str) -> bool {
        self.match_regex.is_match(command)
    }
}

/// Parse a TOML blob and compile every filter in it. A filter that fails to
/// compile is dropped with a warning (fail-safe: the command passes through
/// unfiltered); a blob that fails to parse is an error for the caller.
pub(super) fn parse_and_compile(
    content: &str,
    source: &str,
) -> Result<Vec<CompiledFilter>, String> {
    let file: FilterFile =
        toml::from_str(content).map_err(|e| format!("TOML parse error in {source}: {e}"))?;
    if file.schema_version != 1 {
        return Err(format!(
            "unsupported schema_version {} in {source} (expected 1)",
            file.schema_version
        ));
    }
    let mut compiled = Vec::new();
    for (name, def) in file.filters {
        match compile_filter(name.clone(), def) {
            Ok(f) => compiled.push(f),
            Err(e) => tracing::warn!(filter = %name, %source, error = %e, "bash filter dropped"),
        }
    }
    Ok(compiled)
}

/// Parse a TOML blob without compiling (for the inline-test runner).
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn parse_file(content: &str) -> Result<FilterFile, String> {
    let file: FilterFile = toml::from_str(content).map_err(|e| e.to_string())?;
    if file.schema_version != 1 {
        return Err(format!(
            "unsupported schema_version {}",
            file.schema_version
        ));
    }
    Ok(file)
}

fn compile_filter(name: String, def: FilterDef) -> Result<CompiledFilter, String> {
    if !def.strip_lines_matching.is_empty() && !def.keep_lines_matching.is_empty() {
        return Err("strip_lines_matching and keep_lines_matching are mutually exclusive".into());
    }
    let match_regex =
        Regex::new(&def.match_command).map_err(|e| format!("invalid match_command regex: {e}"))?;
    let replace = def
        .replace
        .into_iter()
        .map(|r| {
            Regex::new(&r.pattern)
                .map(|pattern| CompiledReplaceRule {
                    pattern,
                    replacement: r.replacement,
                })
                .map_err(|e| format!("invalid replace pattern '{}': {e}", r.pattern))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let match_output = def
        .match_output
        .into_iter()
        .map(|r| -> Result<CompiledMatchOutputRule, String> {
            let pattern = Regex::new(&r.pattern)
                .map_err(|e| format!("invalid match_output pattern '{}': {e}", r.pattern))?;
            let unless = Regex::new(&r.unless)
                .map_err(|e| format!("invalid match_output unless '{}': {e}", r.unless))?;
            Ok(CompiledMatchOutputRule {
                pattern,
                message: r.message,
                unless,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let line_filter = if !def.strip_lines_matching.is_empty() {
        LineFilter::Strip(
            RegexSet::new(&def.strip_lines_matching)
                .map_err(|e| format!("invalid strip_lines_matching regex: {e}"))?,
        )
    } else if !def.keep_lines_matching.is_empty() {
        LineFilter::Keep(
            RegexSet::new(&def.keep_lines_matching)
                .map_err(|e| format!("invalid keep_lines_matching regex: {e}"))?,
        )
    } else {
        LineFilter::None
    };
    Ok(CompiledFilter {
        name,
        match_regex,
        strip_ansi: def.strip_ansi,
        replace,
        match_output,
        line_filter,
        truncate_lines_at: def.truncate_lines_at,
        head_lines: def.head_lines,
        tail_lines: def.tail_lines,
        max_lines: def.max_lines,
        on_empty: def.on_empty,
    })
}

/// Truncate a line to `max_len` characters (unicode-safe), appending `...`.
fn truncate_chars(s: &str, max_len: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_len {
        s.to_string()
    } else if max_len < 3 {
        "...".to_string()
    } else {
        format!("{}...", s.chars().take(max_len - 3).collect::<String>())
    }
}

/// Apply a compiled filter to captured output. Pure `&str -> String`.
///
/// `exit_ok` gates every reduction that could drop failure detail: the
/// short-circuit stage, the lossy size caps (truncate/head/tail/max_lines),
/// and the success-flavored `on_empty` message are all applied only when the
/// command actually exited 0.
pub(super) fn apply_filter(filter: &CompiledFilter, output: &str, exit_ok: bool) -> String {
    let mut lines: Vec<String> = output.lines().map(String::from).collect();

    // 1. strip_ansi
    if filter.strip_ansi {
        lines = lines.iter().map(|l| strip_ansi(l)).collect();
    }

    // 2. replace -- line-by-line, rules chained sequentially
    if !filter.replace.is_empty() {
        lines = lines
            .into_iter()
            .map(|mut line| {
                for rule in &filter.replace {
                    line = rule
                        .pattern
                        .replace_all(&line, rule.replacement.as_str())
                        .into_owned();
                }
                line
            })
            .collect();
    }

    // 3. match_output -- short-circuit on whole-blob match (first rule wins).
    //    Skipped entirely on non-zero exit; skipped per-rule when the `unless`
    //    error-guard also matches.
    if exit_ok && !filter.match_output.is_empty() {
        let blob = lines.join("\n");
        for rule in &filter.match_output {
            if rule.pattern.is_match(&blob) && !rule.unless.is_match(&blob) {
                return rule.message.clone();
            }
        }
    }

    // 4. strip OR keep (mutually exclusive). Error/failure lines are exempt:
    //    never stripped, always kept.
    match &filter.line_filter {
        LineFilter::Strip(set) => {
            lines.retain(|l| error_guard().is_match(l) || !set.is_match(l));
        }
        LineFilter::Keep(set) => {
            lines.retain(|l| set.is_match(l) || error_guard().is_match(l));
        }
        LineFilter::None => {}
    }

    // Stages 5-7 are lossy size reduction: they drop or clip lines. ADR-0036
    // "failure is complete" -- skip them on non-zero exit so diagnostics past
    // the caps survive verbatim. The bash tool's `truncate_tail` (mod.rs)
    // remains the final safety backstop for both paths.
    if exit_ok {
        // 5. truncate_lines_at
        if let Some(max_chars) = filter.truncate_lines_at {
            lines = lines
                .into_iter()
                .map(|line| truncate_chars(&line, max_chars))
                .collect();
        }

        // 6. head + tail
        let total = lines.len();
        if let (Some(head), Some(tail)) = (filter.head_lines, filter.tail_lines) {
            if total > head + tail {
                let mut result = lines[..head].to_vec();
                result.push(format!("... ({} lines omitted)", total - head - tail));
                result.extend_from_slice(&lines[total - tail..]);
                lines = result;
            }
        } else if let Some(head) = filter.head_lines
            && total > head
        {
            lines.truncate(head);
            lines.push(format!("... ({} lines omitted)", total - head));
        } else if let Some(tail) = filter.tail_lines
            && total > tail
        {
            let omitted = total - tail;
            lines = lines[omitted..].to_vec();
            lines.insert(0, format!("... ({omitted} lines omitted)"));
        }

        // 7. max_lines -- absolute cap applied after head/tail
        if let Some(max) = filter.max_lines
            && lines.len() > max
        {
            let dropped = lines.len() - max;
            lines.truncate(max);
            lines.push(format!("... ({dropped} lines truncated)"));
        }
    }

    // 8. on_empty -- success-flavored message; only when the command exited 0.
    //    For a failed command emptied by the pipeline, return the empty result
    //    so mod.rs falls back to raw output instead of rendering "ok".
    let result = lines.join("\n");
    if exit_ok
        && result.trim().is_empty()
        && let Some(msg) = &filter.on_empty
    {
        return msg.clone();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_filters(toml: &str) -> Vec<CompiledFilter> {
        parse_and_compile(toml, "test").expect("test TOML should be valid")
    }

    fn first_filter(toml: &str) -> CompiledFilter {
        make_filters(toml)
            .into_iter()
            .next()
            .expect("expected at least one filter")
    }

    fn apply(toml: &str, input: &str) -> String {
        apply_filter(&first_filter(toml), input, true)
    }

    #[test]
    fn strip_ansi_removes_codes() {
        let out = apply(
            "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nstrip_ansi = true\n",
            "\x1b[31mError\x1b[0m\nnormal",
        );
        assert_eq!(out, "Error\nnormal");
    }

    #[test]
    fn replace_rules_chain_sequentially() {
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
replace = [
  { pattern = "foo", replacement = "bar" },
  { pattern = "bar", replacement = "baz" },
]
"#;
        assert_eq!(apply(toml, "foo"), "baz");
    }

    #[test]
    fn strip_lines_matching_basic() {
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_lines_matching = ["^noise", "^verbose"]
"#;
        assert_eq!(
            apply(toml, "noise line\nkeep this\nverbose stuff\nalso keep"),
            "keep this\nalso keep"
        );
    }

    #[test]
    fn keep_lines_matching_basic() {
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
keep_lines_matching = ["^PASS"]
"#;
        assert_eq!(apply(toml, "PASS a\nnoise\nPASS b"), "PASS a\nPASS b");
    }

    #[test]
    fn strip_never_removes_error_lines() {
        // The global error-guard exempts error/failure lines from stripping,
        // even when a (badly authored) strip pattern matches them.
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_lines_matching = [".*"]
"#;
        let input = "chatter\nerror[E0308]: mismatched types\nsrc/x.rs:3:5: error: boom\ntest foo ... FAILED\nthread 'x' panicked at src/lib.rs:9:5\nmore chatter";
        let out = apply(toml, input);
        assert!(out.contains("error[E0308]: mismatched types"), "{out}");
        assert!(out.contains("error: boom"), "{out}");
        assert!(out.contains("test foo ... FAILED"), "{out}");
        assert!(out.contains("panicked at src/lib.rs:9:5"), "{out}");
        assert!(!out.contains("chatter"), "{out}");
    }

    #[test]
    fn keep_always_retains_error_lines() {
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
keep_lines_matching = ["^PASS"]
"#;
        let out = apply(toml, "PASS a\nfatal: repository not found\nnoise");
        assert_eq!(out, "PASS a\nfatal: repository not found");
    }

    #[test]
    fn error_guard_does_not_shield_redundant_count_lines() {
        // "1 error generated." is summary chatter, not the error itself; the
        // guard must leave it strippable (the actual error line is kept).
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_lines_matching = ["^\\d+ errors? generated"]
"#;
        let out = apply(toml, "main.c:1:1: error: boom\n1 error generated.");
        assert_eq!(out, "main.c:1:1: error: boom");
    }

    #[test]
    fn match_output_short_circuits_on_success() {
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
match_output = [
  { pattern = "Build complete", message = "ok", unless = "error:" },
]
"#;
        assert_eq!(apply(toml, "stuff\nBuild complete!\n"), "ok");
    }

    #[test]
    fn match_output_unless_guard_blocks_short_circuit() {
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
match_output = [
  { pattern = "Build complete", message = "ok", unless = "error:" },
]
"#;
        let input = "warning stuff\nerror: bad thing\nBuild complete!";
        assert_eq!(apply(toml, input), input);
    }

    #[test]
    fn match_output_skipped_on_nonzero_exit() {
        // Even a rule whose unless guard does not fire must not replace the
        // output of a failed command with a success message.
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
match_output = [
  { pattern = "done", message = "ok", unless = "error:" },
]
"#;
        let f = first_filter(toml);
        assert_eq!(
            apply_filter(&f, "done (but exit 1)", false),
            "done (but exit 1)"
        );
        assert_eq!(apply_filter(&f, "done", true), "ok");
    }

    #[test]
    fn match_output_first_rule_wins() {
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
match_output = [
  { pattern = "alpha", message = "first", unless = "error:" },
  { pattern = "beta", message = "second", unless = "error:" },
]
"#;
        assert_eq!(apply(toml, "alpha beta"), "first");
    }

    #[test]
    fn truncate_lines_at_is_unicode_safe() {
        let toml =
            "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\ntruncate_lines_at = 5\n";
        assert_eq!(apply(toml, "hello\n日本語xyz"), "hello\n日本...");
    }

    #[test]
    fn head_lines_keeps_prefix_with_marker() {
        let toml = "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nhead_lines = 2\n";
        let out = apply(toml, "a\nb\nc\nd\ne");
        assert!(out.starts_with("a\nb\n"));
        assert!(out.contains("3 lines omitted"));
    }

    #[test]
    fn tail_lines_keeps_suffix_with_marker() {
        let toml = "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\ntail_lines = 2\n";
        let out = apply(toml, "a\nb\nc\nd\ne");
        assert!(out.contains("3 lines omitted"));
        assert!(out.ends_with("d\ne"));
    }

    #[test]
    fn head_and_tail_combined() {
        let toml = "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nhead_lines = 2\ntail_lines = 2\n";
        let out = apply(toml, "a\nb\nc\nd\ne\nf");
        assert!(out.starts_with("a\nb\n"));
        assert!(out.contains("2 lines omitted"));
        assert!(out.ends_with("e\nf"));
    }

    #[test]
    fn max_lines_caps_with_marker() {
        let toml = "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\nmax_lines = 3\n";
        let out = apply(toml, "a\nb\nc\nd\ne");
        assert_eq!(out.lines().count(), 4); // 3 kept + marker
        assert!(out.contains("lines truncated"));
    }

    #[test]
    fn on_empty_fires_when_everything_stripped() {
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_lines_matching = ["^noise"]
on_empty = "nothing left"
"#;
        assert_eq!(apply(toml, "noise a\nnoise b"), "nothing left");
    }

    #[test]
    fn on_empty_not_triggered_when_output_remains() {
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
keep_lines_matching = ["keep"]
on_empty = "nothing left"
"#;
        assert_eq!(apply(toml, "keep this\nnoise"), "keep this");
    }

    #[test]
    fn size_caps_skip_lossy_stages_on_failure() {
        // ADR-0036 "failure is complete": the lossy size-reduction stages
        // (truncate_lines_at, head/tail, max_lines) must not run for a failed
        // command, or diagnostics past the cap are lost.
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
truncate_lines_at = 4
max_lines = 2
"#;
        let f = first_filter(toml);
        let input = "aaaaaaaa\nbbbbbbbb\ncccccccc\nerror: boom past the cap";
        let out = apply_filter(&f, input, false);
        // Nothing dropped, nothing truncated: full output survives verbatim.
        assert_eq!(out, input);
    }

    #[test]
    fn size_caps_apply_on_success() {
        // Success path unchanged: caps still reduce output when exit_ok.
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
truncate_lines_at = 4
max_lines = 2
"#;
        let f = first_filter(toml);
        let input = "aaaaaaaa\nbbbbbbbb\ncccccccc\ndddddddd";
        let out = apply_filter(&f, input, true);
        assert_eq!(out, "a...\nb...\n... (2 lines truncated)");
    }

    #[test]
    fn on_empty_skipped_on_failure() {
        // A failed command whose output is stripped to empty must NOT render
        // the success-flavored on_empty message; the engine returns the empty
        // result so mod.rs falls back to raw output.
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_lines_matching = ["^noise"]
on_empty = "ok (nothing left)"
"#;
        let f = first_filter(toml);
        assert_eq!(apply_filter(&f, "noise a\nnoise b", false), "");
        // Success path unchanged: on_empty still fires when exit_ok.
        assert_eq!(
            apply_filter(&f, "noise a\nnoise b", true),
            "ok (nothing left)"
        );
    }

    #[test]
    fn full_pipeline_order() {
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_ansi = true
strip_lines_matching = ["^noise"]
truncate_lines_at = 10
head_lines = 3
max_lines = 4
on_empty = "empty"
"#;
        let input =
            "\x1b[31mred line\x1b[0m\nnoise skip\nkeep one\nkeep two\nkeep three\nkeep four";
        let out = apply(toml, input);
        assert!(out.contains("red line"));
        assert!(!out.contains("noise skip"));
        assert!(out.contains("lines omitted") || out.contains("lines truncated"));
    }

    #[test]
    fn empty_filter_is_passthrough() {
        let toml = "schema_version = 1\n[filters.f]\nmatch_command = \"^cmd\"\n";
        assert_eq!(apply(toml, "line1\nline2"), "line1\nline2");
    }

    #[test]
    fn strip_and_keep_are_mutually_exclusive() {
        let filters = make_filters(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_lines_matching = ["a"]
keep_lines_matching = ["b"]
"#,
        );
        assert!(filters.is_empty(), "conflicting filter must be dropped");
    }

    #[test]
    fn invalid_regex_drops_filter_not_process() {
        let filters = make_filters("schema_version = 1\n[filters.f]\nmatch_command = \"[\"\n");
        assert!(filters.is_empty());
    }

    #[test]
    fn match_output_without_unless_is_rejected() {
        // Iris requires an error-guard on every short-circuit rule (ADR-0037);
        // a rule without one must fail to deserialize.
        let result = parse_and_compile(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
match_output = [
  { pattern = "done", message = "ok" },
]
"#,
            "test",
        );
        assert!(result.is_err(), "missing unless must be a parse error");
    }

    #[test]
    fn schema_version_mismatch_is_error() {
        assert!(
            parse_and_compile(
                "schema_version = 99\n[filters.f]\nmatch_command = \"^c\"\n",
                "t"
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_field_is_error() {
        assert!(
            parse_and_compile(
                "schema_version = 1\n[filters.f]\nmatch_command = \"^c\"\nstrip_ansi_typo = true\n",
                "t"
            )
            .is_err()
        );
    }

    #[test]
    fn unicode_content_preserved() {
        let toml = r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_lines_matching = ["^noise"]
"#;
        assert_eq!(
            apply(toml, "日本語テスト\nnoise\n中文内容"),
            "日本語テスト\n中文内容"
        );
    }
}
