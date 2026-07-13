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

pub(crate) use super::est_tokens;

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

/// Percentage of estimated tokens removed, given token counts already measured
/// upstream. The compaction seam reports `original_tokens_estimate` /
/// `summary_tokens_estimate` on `CompactionApplied` (same `chars/4` estimator
/// as [`est_tokens`]); a folded covered range has no single verbatim string to
/// re-measure, so the seam's counts are the authoritative before/after. Returns
/// 0.0 for an empty baseline so a degenerate range never divides by zero.
pub(crate) fn reduction_pct_tokens(before: u64, after: u64) -> f64 {
    if before == 0 {
        return 0.0;
    }
    100.0 * (1.0 - after as f64 / before as f64)
}

/// Assert a minimum reduction bar from token counts the seam already measured
/// (see [`reduction_pct_tokens`]). Used where re-deriving a verbatim string is
/// fragile or misleading (the microcompaction arm folds the covered range
/// before compaction, so the covered tokens the seam saw are the honest
/// baseline). Bars are minimums, never exact figures.
pub(crate) fn assert_min_reduction_tokens(class: &str, before: u64, after: u64, min_pct: u32) {
    let pct = reduction_pct_tokens(before, after);
    assert!(
        pct >= f64::from(min_pct),
        "[{class}] token reduction {pct:.1}% is below the {min_pct}% bar ({after} vs {before} \
         est tokens)"
    );
}

/// Assert the reduced form is never larger than the baseline (parity-or-better).
/// Used where the reduction's contract is "at least as small as the raw form"
/// rather than a fixed percentage bar (e.g. grep grouping vs. the ungrouped
/// `path:line:content` baseline). Ratios only; absolute counts are estimates.
pub(crate) fn assert_parity_or_better(class: &str, baseline: &str, reduced: &str) {
    let pct = reduction_pct(baseline, reduced);
    assert!(
        pct >= 0.0,
        "[{class}] reduced output is {}% larger than the baseline ({} vs {} est tokens)",
        -pct as i64,
        est_tokens(reduced),
        est_tokens(baseline),
    );
}

/// Ratio of estimated tokens: `after` as a fraction of `before` (1.0 = no
/// change, < 1.0 = smaller). Complements [`reduction_pct`] for the case where
/// two already-reduced forms are compared against each other (e.g. the
/// compaction `provider` vs `excerpts` arms) rather than raw-vs-reduced.
/// Returns 0.0 when `before` is empty so a degenerate baseline never divides
/// by zero.
pub(crate) fn est_ratio(before: &str, after: &str) -> f64 {
    let before = est_tokens(before) as f64;
    if before == 0.0 {
        return 0.0;
    }
    est_tokens(after) as f64 / before
}

/// Assert `after` stays at or under `max_ratio` of `before` in estimated
/// tokens. Used to bound one reduced arm against another (compaction
/// `provider` vs `excerpts`): the bar is a ceiling on the ratio, i.e. a floor
/// on the win, so a summarizer arm that balloons past the peer fails loudly.
/// Ratios only; absolute counts are estimates.
pub(crate) fn assert_ratio_within(class: &str, before: &str, after: &str, max_ratio: f64) {
    let ratio = est_ratio(before, after);
    assert!(
        ratio <= max_ratio,
        "[{class}] token ratio {ratio:.2} exceeds the {max_ratio:.2} ceiling ({} vs {} est tokens)",
        est_tokens(after),
        est_tokens(before),
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

// ===========================================================================
// Field-wise needle scoring (audit F21) over structured durable-summary text
// (issue #475 / ADR-0061). A whole-text `contains` check (above) cannot see a
// summarizer that drops a fact from its evidenced section and echoes it
// somewhere else, or -- the audit's F17 finding -- one whose injection-defense
// framing silently scrubbed a credential-shaped fact the user explicitly
// asked to keep while every OTHER needle in the bench happened to be
// innocuous-shaped and so never exposed the gap. `assert_survives_fieldwise`
// scores each needle against its declared section(s) whenever `out` has
// detectable `Goal`/`State`/`Decisions`/`Key facts`/`Next steps` structure
// (see `crate::wayland::structured_summary::render_durable_summary`), and
// falls back to the original whole-text check otherwise, so legacy/subagent/
// excerpts summaries keep today's behavior unchanged.
// ===========================================================================

/// One section of a structured durable-summary text. Mirrors the section
/// order `render_durable_summary` renders in; kept independent of that
/// module's `CompactionSummary` type so this test-only scorer has no
/// production-code dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummarySection {
    Goal,
    State,
    Decisions,
    KeyFacts,
    NextSteps,
    /// The ADR-0061 F17 carve-out. Omitted entirely (empty) when the
    /// summarizer preserves nothing there, in which case this section's slice
    /// is always empty and no needle can be found in it.
    PreservedIdentifiers,
}

/// A needle scored by [`assert_survives_fieldwise`]: `text` must appear in at
/// least one of `sections` when `out` is structured. List two or more
/// sections to encode an "either/or" placement rule -- e.g. a
/// credential-shaped fact may land in `KeyFacts` or in the dedicated
/// `PreservedIdentifiers` carve-out; either placement proves retention.
pub(crate) struct FieldNeedle {
    pub(crate) text: &'static str,
    pub(crate) sections: &'static [SummarySection],
}

/// The five mandatory sections plus the optional trailing carve-out, sliced
/// out of one structured durable-summary text. Every slice runs from just
/// after its own header to just before the next header present; the last
/// section found (`next_steps`, or `preserved_identifiers` when present) has
/// no following header to bound it and so extends to the end of the input --
/// callers should pass a bounded summary text, not an entire joined
/// transcript.
struct StructuredSections<'a> {
    goal: &'a str,
    state: &'a str,
    decisions: &'a str,
    key_facts: &'a str,
    next_steps: &'a str,
    preserved_identifiers: &'a str,
}

impl<'a> StructuredSections<'a> {
    fn slice(&self, section: SummarySection) -> &'a str {
        match section {
            SummarySection::Goal => self.goal,
            SummarySection::State => self.state,
            SummarySection::Decisions => self.decisions,
            SummarySection::KeyFacts => self.key_facts,
            SummarySection::NextSteps => self.next_steps,
            SummarySection::PreservedIdentifiers => self.preserved_identifiers,
        }
    }
}

const GOAL_HEADER: &str = "Goal\n";
const STATE_HEADER: &str = "\n\nState";
const DECISIONS_HEADER: &str = "\n\nDecisions";
const KEY_FACTS_HEADER: &str = "\n\nKey facts";
const NEXT_STEPS_HEADER: &str = "\n\nNext steps";
const PRESERVED_IDENTIFIERS_HEADER: &str = "\n\nPreserved identifiers";

/// Detect and slice `render_durable_summary`'s section structure. Headers are
/// searched in order, each scoped to start after the previous header match,
/// so a hit is automatically well-ordered; returns `None` (legacy/non-
/// structured text) the moment any of the five mandatory headers is missing.
fn structured_sections(text: &str) -> Option<StructuredSections<'_>> {
    let goal_start = text.find(GOAL_HEADER)? + GOAL_HEADER.len();
    let state_idx = goal_start + text[goal_start..].find(STATE_HEADER)?;
    let state_start = state_idx + STATE_HEADER.len();
    let decisions_idx = state_start + text[state_start..].find(DECISIONS_HEADER)?;
    let decisions_start = decisions_idx + DECISIONS_HEADER.len();
    let key_facts_idx = decisions_start + text[decisions_start..].find(KEY_FACTS_HEADER)?;
    let key_facts_start = key_facts_idx + KEY_FACTS_HEADER.len();
    let next_steps_idx = key_facts_start + text[key_facts_start..].find(NEXT_STEPS_HEADER)?;
    let next_steps_start = next_steps_idx + NEXT_STEPS_HEADER.len();
    let preserved_idx = text[next_steps_start..]
        .find(PRESERVED_IDENTIFIERS_HEADER)
        .map(|offset| next_steps_start + offset);

    let (next_steps_end, preserved_identifiers) = match preserved_idx {
        Some(idx) => (idx, &text[idx + PRESERVED_IDENTIFIERS_HEADER.len()..]),
        None => (text.len(), ""),
    };

    Some(StructuredSections {
        goal: &text[goal_start..state_idx],
        state: &text[state_start..decisions_idx],
        decisions: &text[decisions_start..key_facts_idx],
        key_facts: &text[key_facts_start..next_steps_idx],
        next_steps: &text[next_steps_start..next_steps_end],
        preserved_identifiers,
    })
}

/// Whether `needle` survives in `out`, scored field-wise when `out` is
/// structured and via the legacy whole-text `contains` check otherwise.
fn needle_survives(out: &str, needle: &FieldNeedle) -> bool {
    match structured_sections(out) {
        Some(sections) => needle
            .sections
            .iter()
            .any(|section| sections.slice(*section).contains(needle.text)),
        None => out.contains(needle.text),
    }
}

/// Assert every needle survives reduction, scored PER SECTION when `out` is a
/// structured durable-summary text and via the whole-text check otherwise
/// (see the module-level rationale above). Never weaker than
/// [`assert_survives_verbatim`] for non-structured text -- the fallback is the
/// same `contains` check -- and strictly tighter for structured text, since a
/// needle present only outside its declared section(s) still fails.
pub(crate) fn assert_survives_fieldwise(class: &str, out: &str, needles: &[FieldNeedle]) {
    for needle in needles {
        assert!(
            needle_survives(out, needle),
            "[{class}] reduced output lost {:?} from its expected section(s)\n--- reduced ---\n{out}",
            needle.text,
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

/// One report row from token counts the seam already measured (see
/// [`reduction_pct_tokens`]). Mirrors [`report_row`] for the compaction arms
/// whose covered range has no single verbatim string to re-measure.
pub(crate) fn report_row_tokens(class: &str, before: u64, after: u64, via: &str) -> String {
    format!(
        "| {class} | {before} | {after} | {:.0}% | {via} |",
        reduction_pct_tokens(before, after),
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

    /// A structured durable-summary text matching
    /// `render_durable_summary`'s exact rendering (issue #475 / ADR-0061):
    /// section headers, "- " bullets, blank-line separators. Pinned here
    /// independently of `wayland::structured_summary::durable_text`'s own
    /// fixture so this test-only scorer stays free of a production-code
    /// dependency; `durable_text.rs`'s
    /// `renders_all_sections_in_order_with_expected_headers` test is the
    /// source of truth for the format both fixtures assume.
    fn structured_fixture() -> String {
        "Goal\nShip the thing\n\n\
         State\n- renderer written\n\n\
         Decisions\n- native first, forced-tool fallback second\n\n\
         Key facts\n- BUILD-7f2a91d\n\n\
         Next steps\n- wire the ladder\n\n\
         Preserved identifiers\n- korium-9741"
            .to_string()
    }

    #[test]
    fn fieldwise_structured_hit_scores_needle_in_its_declared_section() {
        let text = structured_fixture();
        assert_survives_fieldwise(
            "class",
            &text,
            &[FieldNeedle {
                text: "BUILD-7f2a91d",
                sections: &[SummarySection::KeyFacts],
            }],
        );
    }

    #[test]
    #[should_panic(expected = "lost")]
    fn fieldwise_structured_miss_when_needle_only_in_wrong_section() {
        // "BUILD-7f2a91d" lives in Key facts, not Decisions -- the field-wise
        // scorer must not credit it there even though a whole-text `contains`
        // check would pass.
        let text = structured_fixture();
        assert_survives_fieldwise(
            "class",
            &text,
            &[FieldNeedle {
                text: "BUILD-7f2a91d",
                sections: &[SummarySection::Decisions],
            }],
        );
    }

    #[test]
    fn fieldwise_legacy_text_falls_back_to_whole_text_scoring() {
        // No detectable `render_durable_summary` section structure (a
        // colon-joined single-line handoff, like the compaction_bench fake
        // provider's arms) -- scored anywhere in the text, same as
        // `assert_survives_verbatim`, regardless of the declared section.
        let text = "Goal: resume the session. State: edits started. \
                     Key facts: BUILD-7f2a91d; Next: finish the wiring.";
        assert_survives_fieldwise(
            "class",
            text,
            &[FieldNeedle {
                text: "BUILD-7f2a91d",
                sections: &[SummarySection::Decisions],
            }],
        );
    }

    #[test]
    fn fieldwise_credential_needle_passes_via_key_facts_or_preserved_identifiers() {
        let text = structured_fixture();
        assert_survives_fieldwise(
            "class",
            &text,
            &[FieldNeedle {
                text: "korium-9741",
                sections: &[
                    SummarySection::KeyFacts,
                    SummarySection::PreservedIdentifiers,
                ],
            }],
        );
    }

    #[test]
    #[should_panic(expected = "lost")]
    fn fieldwise_credential_needle_fails_when_dropped_from_every_allowed_section() {
        // Reproduces audit F17: an injection-defense framing scrubbed the
        // planted credential from both the `Preserved identifiers` carve-out
        // and `Key facts`. Every other needle in the bench is
        // innocuous-shaped, so a whole-text check would not catch this class
        // of retention failure; the field-wise scorer must.
        let scrubbed = "Goal\nShip the thing\n\n\
             State\n- renderer written\n\n\
             Decisions\n- native first, forced-tool fallback second\n\n\
             Key facts\n- BUILD-7f2a91d\n\n\
             Next steps\n- wire the ladder";
        assert_survives_fieldwise(
            "class",
            scrubbed,
            &[FieldNeedle {
                text: "korium-9741",
                sections: &[
                    SummarySection::KeyFacts,
                    SummarySection::PreservedIdentifiers,
                ],
            }],
        );
    }

    #[test]
    fn est_ratio_and_ceiling() {
        assert_eq!(est_ratio("", "abcd"), 0.0);
        let before = "x".repeat(400);
        let after = "x".repeat(100);
        assert!((est_ratio(&before, &after) - 0.25).abs() < 0.01);
        // At-ceiling passes (<=), well-under passes.
        assert_ratio_within("class", &before, &after, 0.25);
        assert_ratio_within("class", &before, &after, 1.0);
    }

    #[test]
    #[should_panic(expected = "exceeds the 0.10 ceiling")]
    fn ratio_ceiling_fails_loudly() {
        let before = "x".repeat(400);
        let after = "x".repeat(100);
        assert_ratio_within("class", &before, &after, 0.10);
    }

    #[test]
    fn report_row_shape() {
        let row = report_row("c", "aaaaaaaa", "aaaa", "f");
        assert_eq!(row, "| c | 2 | 1 | 50% | f |");
    }

    #[test]
    fn token_reduction_and_row_shape() {
        assert_eq!(reduction_pct_tokens(0, 5), 0.0);
        assert!((reduction_pct_tokens(400, 100) - 75.0).abs() < 0.01);
        let row = report_row_tokens("c", 400, 100, "provider");
        assert_eq!(row, "| c | 400 | 100 | 75% | provider |");
    }

    #[test]
    #[should_panic(expected = "below the 60% bar")]
    fn min_reduction_tokens_bar_fails_loudly() {
        assert_min_reduction_tokens("class", 100, 90, 60);
    }
}
