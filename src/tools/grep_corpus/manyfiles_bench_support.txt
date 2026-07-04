//! Shared test-only helpers for token-efficiency benchmarks (ADR-0036 rule 5:
//! "reduction is measured").
//!
//! Tool-agnostic core of the benchmark recipe: token estimation, reduction
//! ratio, minimum-bar assertion, verbatim-survival assertion (the "zero
//! quality loss" half), warmed per-call overhead bound, and the markdown
//! report rows behind the committed docs under `docs/benchmarks/`.
//!
//! What stays with each consumer: fixture capture, the invocation context
//! that drives the tool's own dispatch (e.g. command string + exit status for
//! the bash filter), and which corpus classes carry which bars. First
//! consumer: `tools::bash::filter::corpus`. Recipe: the
//! `token-efficiency-benchmark` skill (`.pi/skills/`).

use std::time::{Duration, Instant};

/// Rough token estimate: 4 bytes per token, the standard heuristic for
/// English/code text. Benchmarks compare ratios of this estimate; absolute
/// counts are not meaningful.
pub(crate) fn est_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

/// Percentage of estimated tokens removed going from `before` to `after`.
pub(crate) fn reduction_pct(before: &str, after: &str) -> f64 {
    let before = est_tokens(before) as f64;
    let after = est_tokens(after) as f64;
    if before == 0.0 {
        return 0.0;
    }
    100.0 * (1.0 - after / before)
}

/// Assert a minimum reduction bar (percent) for one corpus class. Bars are
/// minimums, never exact figures: exact percentages drift with fixture
/// updates; the bar is the contract.
pub(crate) fn assert_min_reduction(class: &str, before: &str, after: &str, min_pct: u32) {
    let pct = reduction_pct(before, after);
    assert!(
        pct >= f64::from(min_pct),
        "[{class}] token reduction {pct:.1}% is below the {min_pct}% bar\n--- reduced ---\n{after}"
    );
}

/// Assert that every needle survives reduction verbatim. Needles encode the
/// quality-loss contract: error messages, `file:line` references, failing
/// test names, summaries a competent engineer would have read.
pub(crate) fn assert_survives_verbatim(class: &str, out: &str, needles: &[&str]) {
    for needle in needles {
        assert!(
            out.contains(needle),
            "[{class}] reduced output lost {needle:?}\n--- reduced ---\n{out}"
        );
    }
}

/// Assert per-call overhead stays under `bar`. Callers warm any lazy state
/// (compiled registries, caches) before calling; best-of-three absorbs
/// scheduler noise in debug CI runs while still failing on a real regression.
pub(crate) fn assert_call_overhead_under(class: &str, bar: Duration, mut call: impl FnMut()) {
    let best = (0..3)
        .map(|_| {
            let start = Instant::now();
            call();
            start.elapsed()
        })
        .min()
        .expect("three timed runs");
    assert!(
        best < bar,
        "[{class}] per-call overhead {best:?} exceeds the {bar:?} bar"
    );
}

/// Header of the markdown report table committed under `docs/benchmarks/`.
pub(crate) fn report_header() -> String {
    "| class | tokens before | tokens after | reduction | via |\n|---|---|---|---|---|".into()
}

/// One report row. `via` names the reduction path (filter name, tool policy,
/// or `(passthrough)`).
pub(crate) fn report_row(class: &str, before: &str, after: &str, via: &str) -> String {
    format!(
        "| {class} | {} | {} | {:.0}% | {via} |",
        est_tokens(before),
        est_tokens(after),
        reduction_pct(before, after),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn est_tokens_rounds_up() {
        assert_eq!(est_tokens(""), 0);
        assert_eq!(est_tokens("abc"), 1);
        assert_eq!(est_tokens("abcd"), 1);
        assert_eq!(est_tokens("abcde"), 2);
    }

    #[test]
    fn reduction_pct_basic() {
        assert_eq!(reduction_pct("", ""), 0.0);
        let before = "x".repeat(400);
        let after = "x".repeat(100);
        let pct = reduction_pct(&before, &after);
        assert!((pct - 75.0).abs() < 0.01, "{pct}");
    }

    #[test]
    #[should_panic(expected = "below the 60% bar")]
    fn min_reduction_bar_fails_loudly() {
        assert_min_reduction("class", "aaaabbbb", "aaaabbb", 60);
    }

    #[test]
    #[should_panic(expected = "lost")]
    fn survival_assert_fails_loudly() {
        assert_survives_verbatim("class", "kept line", &["dropped line"]);
    }

    #[test]
    fn report_row_shape() {
        let row = report_row("c", "aaaaaaaa", "aaaa", "f");
        assert_eq!(row, "| c | 2 | 1 | 50% | f |");
    }
}
