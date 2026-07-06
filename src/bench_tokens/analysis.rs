//! Phase 7: deterministic analysis of the tokens-per-task JSONL log.
//!
//! Pure functions over the schema-v3 records the runner writes -- aggregate the
//! N runs per `(model, workload, arm)`, pair arm A (defaults) vs B (baseline),
//! decompose the input-token delta, and emit an honesty verdict. No provider,
//! no network: the gate test feeds synthetic records; the opt-in report reader
//! points this at a real run's log.
//!
//! Token-source discipline (never mixed): absolute deltas come ONLY from
//! `real_cell.input_tokens` (real usage records). `render_probe` proxy tokens
//! are reported in their own section as ratios and never combined with a real
//! figure. The "did the reduction actually fire in context" signal is the
//! `tool_result_bytes` delta -- real bytes measured in BOTH arms, so it shares
//! units with neither proxy nor usage tokens and mixes nothing.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Deserialize;

use super::arms::Arm;

/// Below this many paired runs per arm, a token delta is descriptive only --
/// never a claim (the honesty rule: small N => inconclusive).
const MIN_N_FOR_CLAIM: usize = 5;

/// One JSONL record, parsed tolerantly: unknown fields are ignored and missing
/// fields default, so a reader survives a newer writer (ADR-0036 schema rule).
#[derive(Debug, Deserialize)]
struct RawRecord {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    valid: bool,
    #[serde(default)]
    model: String,
    #[serde(default)]
    workload: String,
    #[serde(default)]
    reduce_output: Option<bool>,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    turns: Option<u64>,
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    tool_result_bytes: Option<u64>,
    #[serde(default)]
    tool_calls_total: Option<u64>,
    #[serde(default)]
    tool_errors: Vec<serde_json::Value>,
    // render_probe fields
    #[serde(default)]
    tool: String,
    #[serde(default)]
    probe: String,
    #[serde(default)]
    reduction_pct: Option<f64>,
    #[serde(default)]
    needles_survived: Option<bool>,
    #[serde(default)]
    baseline_proxy_tokens: Option<u64>,
    #[serde(default)]
    reduced_proxy_tokens: Option<u64>,
}

/// A usable real-provider cell (one run).
#[derive(Clone)]
struct Cell {
    arm: Arm,
    success: bool,
    input_tokens: u64,
    turns: u64,
    tool_result_bytes: u64,
    tool_calls_total: u64,
    tool_errors: u64,
}

/// A render-probe measurement (proxy tokens; reported separately, never mixed).
#[derive(Clone)]
pub(crate) struct RenderRow {
    pub(crate) probe: String,
    pub(crate) tool: String,
    pub(crate) reduction_pct: f64,
    pub(crate) needles_survived: bool,
    pub(crate) proxy_saved: i64,
}

/// The honesty verdict for one paired comparison (or the whole run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// No paired data to judge.
    NoData,
    /// One arm is missing -- the pair cannot be compared.
    Incomplete,
    /// A cheaper reduced arm, but N is too small or the spreads overlap.
    Inconclusive,
    /// Baseline ties or wins on tokens -- do NOT claim a reduction.
    BaselineWins,
    /// Reduced arm dropped task success vs baseline -- STOP AND REPORT.
    SuccessRegression,
    /// Reduced arm cheaper, success held, spreads clear, N adequate.
    Supported,
}

impl Verdict {
    /// Shipping-block rank: higher is more blocking. The overall verdict is the
    /// most-blocking pairing verdict, so one regression fails the whole run.
    fn rank(self) -> u8 {
        match self {
            Verdict::Supported => 0,
            Verdict::NoData => 1,
            Verdict::Incomplete => 2,
            Verdict::Inconclusive => 3,
            Verdict::BaselineWins => 4,
            Verdict::SuccessRegression => 5,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Verdict::NoData => "NO DATA",
            Verdict::Incomplete => "INCOMPLETE (one arm missing)",
            Verdict::Inconclusive => "INCONCLUSIVE (small N or overlapping spread)",
            Verdict::BaselineWins => "BASELINE WINS (no claim)",
            Verdict::SuccessRegression => "SUCCESS REGRESSION (stop and report)",
            Verdict::Supported => "SUPPORTED (descriptive; still needs N)",
        }
    }
}

/// Per-arm aggregate for one `(model, workload)`.
struct ArmStats {
    n: usize,
    successes: usize,
    input: Vec<u64>,
    turns: Vec<u64>,
    bytes: Vec<u64>,
    tool_calls: Vec<u64>,
    tool_errors: Vec<u64>,
}

impl ArmStats {
    fn new() -> Self {
        Self {
            n: 0,
            successes: 0,
            input: Vec::new(),
            turns: Vec::new(),
            bytes: Vec::new(),
            tool_calls: Vec::new(),
            tool_errors: Vec::new(),
        }
    }

    fn push(&mut self, cell: &Cell) {
        self.n += 1;
        if cell.success {
            self.successes += 1;
        }
        self.input.push(cell.input_tokens);
        self.turns.push(cell.turns);
        self.bytes.push(cell.tool_result_bytes);
        self.tool_calls.push(cell.tool_calls_total);
        self.tool_errors.push(cell.tool_errors);
    }

    fn success_rate(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.successes as f64 / self.n as f64
        }
    }
}

/// One paired A-vs-B comparison for a `(model, workload)`.
pub(crate) struct Pairing {
    pub(crate) model: String,
    pub(crate) workload: String,
    pub(crate) n_a: usize,
    pub(crate) n_b: usize,
    pub(crate) success_a: f64,
    pub(crate) success_b: f64,
    pub(crate) median_in_a: u64,
    pub(crate) median_in_b: u64,
    pub(crate) delta_input: i64,
    pub(crate) delta_input_pct: f64,
    pub(crate) median_turns_a: f64,
    pub(crate) median_turns_b: f64,
    pub(crate) median_tool_calls_a: f64,
    pub(crate) median_tool_calls_b: f64,
    pub(crate) max_tool_calls_a: u64,
    pub(crate) max_tool_calls_b: u64,
    pub(crate) total_tool_errors_a: u64,
    pub(crate) total_tool_errors_b: u64,
    /// Decomposition of the input delta: cheaper-per-turn vs changed turn count.
    /// NOTE: `tpt = cumulative_input / turns` is an AVERAGE over a growing
    /// series (each turn resends the transcript), so when the arms take a
    /// different number of turns this split is confounded -- see `mechanism()`.
    pub(crate) term_efficiency: f64,
    pub(crate) term_turns: f64,
    /// Real tool-result bytes delta (A - B), the "reduction fired?" signal.
    pub(crate) delta_result_bytes: i64,
    /// Welch 95% CI on the mean input-token saving (B - A; positive = A cheaper).
    pub(crate) mean_saving: f64,
    pub(crate) ci_low: f64,
    pub(crate) ci_high: f64,
    /// The saving CI lies entirely above zero -- a statistically defensible
    /// reduction (the SUPPORTED gate, with success held + adequate N).
    pub(crate) significant: bool,
    pub(crate) verdict: Verdict,
}

impl Pairing {
    /// Where a token delta actually came from. When the arms took the same
    /// number of turns, a delta is a genuine per-turn (reduction) effect. When
    /// they differ, the delta is dominated by whole eliminated/added turns
    /// (each ~fixed system-prompt + tool-schema overhead), a STRATEGY difference
    /// confounded with the reduction -- NOT evidence the reduction made turns
    /// cheaper. Reported so the eff/turns split is never over-read.
    pub(crate) fn mechanism(&self) -> &'static str {
        if (self.median_turns_a - self.median_turns_b).abs() < 0.5 {
            "per-turn (same turn count)"
        } else if self.delta_input < 0 {
            "fewer turns (confounded w/ strategy)"
        } else {
            "more turns (confounded w/ strategy)"
        }
    }
}

/// The whole-run analysis.
pub(crate) struct Analysis {
    pub(crate) cell_count: usize,
    pub(crate) invalid_count: usize,
    pub(crate) error_count: usize,
    pub(crate) skipped_lines: usize,
    pub(crate) render_rows: Vec<RenderRow>,
    pub(crate) pairings: Vec<Pairing>,
    pub(crate) overall: Verdict,
}

fn median(values: &[u64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        v[mid] as f64
    } else {
        (v[mid - 1] + v[mid]) as f64 / 2.0
    }
}

fn max(values: &[u64]) -> u64 {
    values.iter().copied().max().unwrap_or(0)
}

/// Arithmetic mean of a sample.
fn mean(values: &[u64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<u64>() as f64 / values.len() as f64
}

/// Unbiased (n-1) sample variance about `m`.
fn sample_var(values: &[u64], m: f64) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let ss: f64 = values.iter().map(|&x| (x as f64 - m).powi(2)).sum();
    ss / (values.len() as f64 - 1.0)
}

/// Welch 95% confidence interval on the mean input-token saving `mean_b -
/// mean_a` (positive = the reduced arm A is cheaper). Normal-approximation
/// (z=1.96) two-sample interval with unequal variances -- the correct test for
/// "is the saving distinguishable from zero" at moderate+ N. It REPLACES the old
/// range-overlap guard, which can never certify at large N (one outlier A-run
/// always exceeds the cheapest B-run) yet was the only Supported gate. Returns
/// (saving, lo, hi).
fn welch_ci(a: &[u64], b: &[u64]) -> (f64, f64, f64) {
    let (ma, mb) = (mean(a), mean(b));
    let saving = mb - ma;
    let se = (sample_var(a, ma) / a.len().max(1) as f64
        + sample_var(b, mb) / b.len().max(1) as f64)
        .sqrt();
    (saving, saving - 1.96 * se, saving + 1.96 * se)
}

/// Parse and analyze a JSONL log body. Blank and unparsable lines are counted
/// (`skipped_lines`) rather than aborting -- a partial log still yields a
/// partial verdict.
pub(crate) fn analyze_jsonl(body: &str) -> Analysis {
    let mut cells: BTreeMap<(String, String), Vec<Cell>> = BTreeMap::new();
    let mut render_rows = Vec::new();
    let mut cell_count = 0;
    let mut invalid_count = 0;
    let mut error_count = 0;
    let mut skipped_lines = 0;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<RawRecord>(trimmed) else {
            skipped_lines += 1;
            continue;
        };
        match rec.kind.as_str() {
            "render_probe" => render_rows.push(RenderRow {
                probe: rec.probe,
                tool: rec.tool,
                reduction_pct: rec.reduction_pct.unwrap_or(0.0),
                needles_survived: rec.needles_survived.unwrap_or(false),
                proxy_saved: rec.baseline_proxy_tokens.unwrap_or(0) as i64
                    - rec.reduced_proxy_tokens.unwrap_or(0) as i64,
            }),
            "real_cell_error" => error_count += 1,
            "real_cell" => {
                // A cell is usable only with a real usage record (input_tokens
                // present and non-zero -- usage None => invalid) and the fields
                // the pairing needs. Everything else is counted, never dropped.
                match (rec.reduce_output, rec.success, rec.turns, rec.input_tokens) {
                    (Some(reduce), Some(success), Some(turns), Some(input))
                        if rec.valid && input > 0 && turns > 0 =>
                    {
                        cell_count += 1;
                        let arm = if reduce { Arm::Defaults } else { Arm::Baseline };
                        cells
                            .entry((rec.model, rec.workload))
                            .or_default()
                            .push(Cell {
                                arm,
                                success,
                                input_tokens: input,
                                turns,
                                tool_result_bytes: rec.tool_result_bytes.unwrap_or(0),
                                tool_calls_total: rec.tool_calls_total.unwrap_or(0),
                                tool_errors: rec.tool_errors.len() as u64,
                            });
                    }
                    _ => invalid_count += 1,
                }
            }
            _ => skipped_lines += 1,
        }
    }

    let mut pairings: Vec<Pairing> = cells
        .into_iter()
        .map(|((model, workload), group)| pair(model, workload, &group))
        .collect();
    pairings.sort_by(|a, b| (&a.model, &a.workload).cmp(&(&b.model, &b.workload)));

    let overall = pairings
        .iter()
        .map(|p| p.verdict)
        .max_by_key(|v| v.rank())
        .unwrap_or(Verdict::NoData);

    Analysis {
        cell_count,
        invalid_count,
        error_count,
        skipped_lines,
        render_rows,
        pairings,
        overall,
    }
}

/// Build one paired comparison + its verdict from a group's cells.
fn pair(model: String, workload: String, group: &[Cell]) -> Pairing {
    let mut a = ArmStats::new();
    let mut b = ArmStats::new();
    for cell in group {
        match cell.arm {
            Arm::Defaults => a.push(cell),
            Arm::Baseline => b.push(cell),
        }
    }

    let median_in_a = median(&a.input);
    let median_in_b = median(&b.input);
    let delta_input = median_in_a as i64 - median_in_b as i64;
    let delta_input_pct = if median_in_b > 0.0 {
        100.0 * (median_in_a - median_in_b) / median_in_b
    } else {
        0.0
    };

    // Token-delta decomposition: separate "each turn got cheaper" (the
    // reduction working) from "the arm changed how many turns it took" (a
    // strategy change). delta = turns_a*(tpt_a - tpt_b) + (turns_a-turns_b)*tpt_b.
    let turns_a = median(&a.turns);
    let turns_b = median(&b.turns);
    let median_tool_calls_a = median(&a.tool_calls);
    let median_tool_calls_b = median(&b.tool_calls);
    let max_tool_calls_a = max(&a.tool_calls);
    let max_tool_calls_b = max(&b.tool_calls);
    let total_tool_errors_a = a.tool_errors.iter().sum();
    let total_tool_errors_b = b.tool_errors.iter().sum();
    let tpt_a = if turns_a > 0.0 {
        median_in_a / turns_a
    } else {
        0.0
    };
    let tpt_b = if turns_b > 0.0 {
        median_in_b / turns_b
    } else {
        0.0
    };
    let term_efficiency = turns_a * (tpt_a - tpt_b);
    let term_turns = (turns_a - turns_b) * tpt_b;

    let delta_result_bytes = median(&a.bytes) as i64 - median(&b.bytes) as i64;
    let (mean_saving, ci_low, ci_high) = welch_ci(&a.input, &b.input);
    let significant = ci_low > 0.0;

    let verdict = verdict_for(&a, &b, delta_input, significant);

    Pairing {
        model,
        workload,
        n_a: a.n,
        n_b: b.n,
        success_a: a.success_rate(),
        success_b: b.success_rate(),
        median_in_a: median_in_a.round() as u64,
        median_in_b: median_in_b.round() as u64,
        delta_input,
        delta_input_pct,
        median_turns_a: turns_a,
        median_turns_b: turns_b,
        median_tool_calls_a,
        median_tool_calls_b,
        max_tool_calls_a,
        max_tool_calls_b,
        total_tool_errors_a,
        total_tool_errors_b,
        term_efficiency,
        term_turns,
        delta_result_bytes,
        mean_saving,
        ci_low,
        ci_high,
        significant,
        verdict,
    }
}

/// The honesty verdict, in precedence order (most-blocking first).
fn verdict_for(a: &ArmStats, b: &ArmStats, delta_input: i64, significant: bool) -> Verdict {
    if a.n == 0 || b.n == 0 {
        return Verdict::Incomplete;
    }
    // A drop in task success is the headline regardless of N or tokens.
    if a.success_rate() + f64::EPSILON < b.success_rate() {
        return Verdict::SuccessRegression;
    }
    // Baseline ties or wins on the (robust, median) token delta: nothing to claim.
    if delta_input >= 0 {
        return Verdict::BaselineWins;
    }
    // Cheaper reduced arm: defensible only with adequate N AND a saving whose
    // Welch 95% CI clears zero. Small N or a CI that crosses zero stays
    // inconclusive (descriptive only) -- this is what keeps a noisy cell (e.g.
    // a model with high turn-count variance) from being called a win.
    if a.n.min(b.n) < MIN_N_FOR_CLAIM || !significant {
        return Verdict::Inconclusive;
    }
    Verdict::Supported
}

/// Render the analysis as a Markdown report (the committed artifact form).
pub(crate) fn format_report(analysis: &Analysis) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Tokens-per-task analysis\n");
    let _ = writeln!(
        out,
        "Cells: {} valid, {} invalid (usage None / missing fields), {} errored, {} lines skipped.\n",
        analysis.cell_count, analysis.invalid_count, analysis.error_count, analysis.skipped_lines
    );
    let _ = writeln!(out, "OVERALL VERDICT: {}\n", analysis.overall.label());

    if !analysis.render_rows.is_empty() {
        let _ = writeln!(
            out,
            "## Render probes (proxy tokens; ratios only, never mixed with real usage)\n"
        );
        let _ = writeln!(
            out,
            "| probe | tool | reduction | needles | proxy tokens saved |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|");
        for r in &analysis.render_rows {
            let _ = writeln!(
                out,
                "| {} | {} | {:.1}% | {} | {} |",
                r.probe,
                r.tool,
                r.reduction_pct,
                if r.needles_survived {
                    "survived"
                } else {
                    "LOST"
                },
                r.proxy_saved
            );
        }
        let _ = writeln!(out);
    }

    if analysis.pairings.is_empty() {
        let _ = writeln!(out, "No paired real-provider cells to compare.");
        return out;
    }

    let _ = writeln!(
        out,
        "## Paired A (defaults) vs B (baseline) -- real usage tokens\n"
    );
    let _ = writeln!(
        out,
        "| model | workload | N a/b | success a/b | med in a/b | turns a/b | delta | mechanism | eff / turns | result-bytes delta | verdict |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|---|---|");
    for p in &analysis.pairings {
        let _ = writeln!(
            out,
            "| {} | {} | {}/{} | {:.0}%/{:.0}% | {}/{} | {:.0}/{:.0} | {:+} ({:+.1}%) | {} | {:+.0} / {:+.0} | {:+} | {} |",
            p.model,
            p.workload,
            p.n_a,
            p.n_b,
            p.success_a * 100.0,
            p.success_b * 100.0,
            p.median_in_a,
            p.median_in_b,
            p.median_turns_a,
            p.median_turns_b,
            p.delta_input,
            p.delta_input_pct,
            p.mechanism(),
            p.term_efficiency,
            p.term_turns,
            p.delta_result_bytes,
            p.verdict.label(),
        );
    }
    let _ = writeln!(
        out,
        "\n`delta` is A - B median input tokens (negative = defaults cheaper). \
         `mechanism` says where it came from: `per-turn` (same turn count -- a genuine \
         reduction effect) or `fewer/more turns` (dominated by whole eliminated/added \
         turns of mostly-fixed prompt overhead, a STRATEGY difference confounded with \
         the reduction). `eff / turns` is the arithmetic split, but because per-turn \
         tokens are cumulative it is a clean reduction signal ONLY when turn counts \
         match. `result-bytes delta` is real tool-output bytes in context (A - B); ~0 \
         means the reduction never fired for that cell's tool path."
    );

    let _ = writeln!(out, "\n## Safety / loop signals\n");
    let _ = writeln!(
        out,
        "| model | workload | success a/b | turns a/b | tool calls med a/b | tool calls max a/b | tool errors a/b |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|");
    for p in &analysis.pairings {
        let _ = writeln!(
            out,
            "| {} | {} | {:.0}%/{:.0}% | {:.0}/{:.0} | {:.1}/{:.1} | {}/{} | {}/{} |",
            p.model,
            p.workload,
            p.success_a * 100.0,
            p.success_b * 100.0,
            p.median_turns_a,
            p.median_turns_b,
            p.median_tool_calls_a,
            p.median_tool_calls_b,
            p.max_tool_calls_a,
            p.max_tool_calls_b,
            p.total_tool_errors_a,
            p.total_tool_errors_b,
        );
    }
    let _ = writeln!(
        out,
        "\nThis section is the N-run compaction-safety check: if defaults keep the same \
         success rate without higher turns, higher tool-call maxima, or a tool-error \
         spike, the reduced output did not make the task harder to interpret or \
         trigger tool loops for this workload."
    );

    let _ = writeln!(
        out,
        "\n## Significance (Welch 95% CI on mean input-token saving, B - A; + = defaults cheaper)\n"
    );
    let _ = writeln!(
        out,
        "| model | workload | mean saving | 95% CI | clears zero |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|");
    for p in &analysis.pairings {
        let _ = writeln!(
            out,
            "| {} | {} | {:+.0} | [{:+.0}, {:+.0}] | {} |",
            p.model,
            p.workload,
            p.mean_saving,
            p.ci_low,
            p.ci_high,
            if p.significant { "yes" } else { "no" },
        );
    }
    let _ = writeln!(
        out,
        "\nA cell is SUPPORTED only when its saving CI clears zero, success held, \
         and N is adequate -- a real, statistically defensible reduction. A CI that \
         crosses zero stays INCONCLUSIVE no matter how large N is."
    );
    out
}

/// Convenience for the opt-in report reader: analyze a log file and render it.
#[cfg(test)]
pub(crate) fn report_from_file(path: &str) -> std::io::Result<String> {
    let body = std::fs::read_to_string(path)?;
    Ok(format_report(&analyze_jsonl(&body)))
}
