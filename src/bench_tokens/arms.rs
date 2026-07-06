//! Benchmark arms: Iris defaults (reductions on) vs the benchmark-only baseline
//! (default-on reductions disabled). The ONLY arm selector is the test-only
//! `ToolState::with_reduce_output(bool)`; production is always reduce-on.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arm {
    /// Iris defaults: bash filter, grep grouping, find grouping active.
    Defaults,
    /// Baseline: default-on reductions disabled (the benchmark-only switch).
    Baseline,
}

impl Arm {
    /// Whether tool output reductions are active for this arm.
    pub(crate) fn reduce(self) -> bool {
        matches!(self, Arm::Defaults)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Arm::Defaults => "A (defaults)",
            Arm::Baseline => "B (baseline)",
        }
    }
}
