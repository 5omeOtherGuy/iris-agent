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
}

impl ArmStats {
    fn new() -> Self {
        Self {
            n: 0,
            successes: 0,
            input: Vec::new(),
            turns: Vec::new(),
            bytes: Vec::new(),
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
    /// Decomposition of the input delta: cheaper-per-turn vs changed turn count.
    pub(crate) term_efficiency: f64,
    pub(crate) term_turns: f64,
    /// Real tool-result bytes delta (A - B), the "reduction fired?" signal.
    pub(crate) delta_result_bytes: i64,
    pub(crate) verdict: Verdict,
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

fn spread(values: &[u64]) -> (u64, u64) {
    let min = values.iter().copied().min().unwrap_or(0);
    let max = values.iter().copied().max().unwrap_or(0);
    (min, max)
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

    let verdict = verdict_for(&a, &b, delta_input);

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
        term_efficiency,
        term_turns,
        delta_result_bytes,
        verdict,
    }
}

/// The honesty verdict, in precedence order (most-blocking first).
fn verdict_for(a: &ArmStats, b: &ArmStats, delta_input: i64) -> Verdict {
    if a.n == 0 || b.n == 0 {
        return Verdict::Incomplete;
    }
    // A drop in task success is the headline regardless of N or tokens.
    if a.success_rate() + f64::EPSILON < b.success_rate() {
        return Verdict::SuccessRegression;
    }
    // Baseline ties or wins on tokens: no reduction to claim.
    if delta_input >= 0 {
        return Verdict::BaselineWins;
    }
    // Cheaper reduced arm, but not defensible: small N or overlapping spreads
    // (the small-N stand-in for a CI that crosses zero).
    let (_, a_max) = spread(&a.input);
    let (b_min, _) = spread(&b.input);
    if a.n.min(b.n) < MIN_N_FOR_CLAIM || a_max >= b_min {
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
        "| model | workload | N a/b | success a/b | med in a/b | delta | eff / turns | result-bytes delta | verdict |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|");
    for p in &analysis.pairings {
        let _ = writeln!(
            out,
            "| {} | {} | {}/{} | {:.0}%/{:.0}% | {}/{} | {:+} ({:+.1}%) | {:+.0} / {:+.0} | {:+} | {} |",
            p.model,
            p.workload,
            p.n_a,
            p.n_b,
            p.success_a * 100.0,
            p.success_b * 100.0,
            p.median_in_a,
            p.median_in_b,
            p.delta_input,
            p.delta_input_pct,
            p.term_efficiency,
            p.term_turns,
            p.delta_result_bytes,
            p.verdict.label(),
        );
    }
    let _ = writeln!(
        out,
        "\n`delta` is A - B median input tokens (negative = defaults cheaper). \
         `eff / turns` decomposes it into cheaper-per-turn vs changed turn count. \
         `result-bytes delta` is real tool-output bytes in context (A - B); ~0 means \
         the reduction never fired for that cell's tool path."
    );
    out
}

/// Convenience for the opt-in report reader: analyze a log file and render it.
#[cfg(test)]
pub(crate) fn report_from_file(path: &str) -> std::io::Result<String> {
    let body = std::fs::read_to_string(path)?;
    Ok(format_report(&analyze_jsonl(&body)))
}
