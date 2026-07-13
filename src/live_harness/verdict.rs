//! Live-run verdict machinery, promoted out of `compaction_live_bench.rs`
//! (issue #545). Classifies each run into pass / flaky-exclusion / hard-failure
//! purely from its gate results, then aggregates an ordered list of outcomes
//! under a single shared exclusion budget. Pure and deterministic so the
//! exclusion rule is unit-tested without any live traffic.

/// At most one flaky-session exclusion per run (see
/// `docs/benchmarks/auto-compaction-live-loop.md`). Error-based exclusions and
/// timing-flake exclusions share this single budget.
pub(crate) const LIVE_EXCLUSION_BUDGET: usize = 1;

/// The per-session gate outcome, classified independently of live traffic so a
/// deterministic unit test can pin the flaky-exclusion decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveSessionOutcome {
    /// Every gate passed.
    Pass,
    /// The only failing gate is the timing gate (non-hard main-loop blocking).
    /// Eligible for the run's single permitted flaky exclusion while the budget
    /// is free.
    G1TimingFlake,
    /// A non-timing gate failed; never eligible for the flaky exclusion.
    HardFailure,
    /// The session raised a provider/stream/auth error before producing a row.
    /// Consumes the same one-per-run budget as a timing flake.
    ErrorExclusion,
}

/// The boolean gate results for one scripted session, extracted so the
/// classification is a pure function of the gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LiveSessionGates {
    /// At least two real auto-compactions were forced.
    pub(crate) two_compactions: bool,
    /// Non-hard main-loop blocking stayed under budget (the timing gate).
    pub(crate) g1_non_blocking: bool,
    /// Post-apply context estimate stayed effective.
    pub(crate) context_effective: bool,
    /// The planted needle was answered.
    pub(crate) needle_answered: bool,
    /// The planted credential-shaped needle (audit F17/F21: a password-like
    /// fact the scripted user explicitly asked to remember) was answered.
    /// Scored the same way as `needle_answered`; kept as its own field rather
    /// than folded into it so a regression that drops only the credential
    /// (the exact F17 failure mode) is distinguishable in the failure detail.
    pub(crate) credential_answered: bool,
    /// One recall marker per compaction.
    pub(crate) recall_marker: bool,
    /// The deterministic carry block was retained.
    pub(crate) carry_block: bool,
    /// Resumed context matched live byte-for-byte.
    pub(crate) resume_exact: bool,
    /// Every entry recorded the required metadata.
    pub(crate) measured_entries: bool,
    /// A real repository read executed.
    pub(crate) real_read: bool,
}

impl LiveSessionGates {
    /// True when every gate other than the timing gate passed. A timing-only
    /// failure is the sole shape eligible for the flaky exclusion.
    fn non_timing_gates_pass(self) -> bool {
        self.two_compactions
            && self.context_effective
            && self.needle_answered
            && self.credential_answered
            && self.recall_marker
            && self.carry_block
            && self.resume_exact
            && self.measured_entries
            && self.real_read
    }
}

/// Classify one session purely from its gate results. A row is a flaky-exclusion
/// candidate only when the timing gate is its single failing gate.
pub(crate) fn classify_live_gates(gates: LiveSessionGates) -> LiveSessionOutcome {
    match (gates.non_timing_gates_pass(), gates.g1_non_blocking) {
        (true, true) => LiveSessionOutcome::Pass,
        (true, false) => LiveSessionOutcome::G1TimingFlake,
        (false, _) => LiveSessionOutcome::HardFailure,
    }
}

/// The run-level verdict after applying the single shared flaky-exclusion
/// budget to an ordered list of session outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LiveRunVerdict {
    pub(crate) passed: bool,
    pub(crate) exclusions: usize,
}

/// Aggregate a run's ordered session outcomes under the one-per-run budget that
/// error-based and timing exclusions share. Both exclusion kinds count equally
/// against the shared budget, so `exclusions` is the total number of excluded
/// sessions regardless of order. The run passes only when no hard gate failure
/// occurred and the exclusion count stays within budget; a second exclusion of
/// either kind therefore fails the run.
pub(crate) fn live_run_verdict(outcomes: &[LiveSessionOutcome]) -> LiveRunVerdict {
    live_run_verdict_with_budget(outcomes, LIVE_EXCLUSION_BUDGET)
}

/// As [`live_run_verdict`], but with a caller-supplied exclusion budget so a
/// config campaign can widen or tighten the flaky tolerance. The default budget
/// (`LIVE_EXCLUSION_BUDGET`) is used when a config omits it.
pub(crate) fn live_run_verdict_with_budget(
    outcomes: &[LiveSessionOutcome],
    budget: usize,
) -> LiveRunVerdict {
    let mut exclusions = 0usize;
    let mut hard_failures = 0usize;
    for outcome in outcomes {
        match outcome {
            LiveSessionOutcome::Pass => {}
            LiveSessionOutcome::HardFailure => hard_failures += 1,
            LiveSessionOutcome::ErrorExclusion | LiveSessionOutcome::G1TimingFlake => {
                exclusions += 1;
            }
        }
    }
    LiveRunVerdict {
        passed: hard_failures == 0 && exclusions <= budget,
        exclusions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_pass() -> LiveSessionGates {
        LiveSessionGates {
            two_compactions: true,
            g1_non_blocking: true,
            context_effective: true,
            needle_answered: true,
            credential_answered: true,
            recall_marker: true,
            carry_block: true,
            resume_exact: true,
            measured_entries: true,
            real_read: true,
        }
    }

    /// A session whose only failing gate is the timing gate is the run's single
    /// permitted flaky exclusion while the budget is free, and the run passes.
    #[test]
    fn g1_timing_flake_excluded_when_exclusion_budget_is_free() {
        let flake = classify_live_gates(LiveSessionGates {
            g1_non_blocking: false,
            ..all_pass()
        });
        assert_eq!(flake, LiveSessionOutcome::G1TimingFlake);

        let verdict =
            live_run_verdict(&[LiveSessionOutcome::Pass, flake, LiveSessionOutcome::Pass]);
        assert!(verdict.passed, "a lone timing flake must be excluded");
        assert_eq!(verdict.exclusions, 1);
    }

    /// The error and timing exclusions share one budget, so a timing flake
    /// following an error exclusion is over budget and fails the run.
    #[test]
    fn g1_timing_flake_fails_run_when_budget_already_spent_on_error() {
        let verdict = live_run_verdict(&[
            LiveSessionOutcome::ErrorExclusion,
            LiveSessionOutcome::G1TimingFlake,
        ]);
        assert!(
            !verdict.passed,
            "error and timing flake share one budget; the second exclusion fails the run"
        );
        assert_eq!(verdict.exclusions, 2);
    }

    /// A row failing the timing gate plus any other gate is a hard failure.
    #[test]
    fn g1_plus_another_gate_failure_is_not_excludable() {
        let outcome = classify_live_gates(LiveSessionGates {
            g1_non_blocking: false,
            resume_exact: false,
            ..all_pass()
        });
        assert_eq!(outcome, LiveSessionOutcome::HardFailure);
        assert!(!live_run_verdict(&[outcome]).passed);
    }

    /// Only one flaky exclusion is permitted per run, so two timing flakes fail.
    #[test]
    fn two_g1_timing_flakes_fail_the_run() {
        let verdict = live_run_verdict(&[
            LiveSessionOutcome::G1TimingFlake,
            LiveSessionOutcome::G1TimingFlake,
        ]);
        assert!(
            !verdict.passed,
            "only one flaky exclusion is permitted per run"
        );
        assert_eq!(verdict.exclusions, 2);
    }
}
