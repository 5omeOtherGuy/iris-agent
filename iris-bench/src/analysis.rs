//! Offline analysis: read a JSONL run log and aggregate per
//! (model × reasoning × workload × arm), pairing the Defaults vs Baseline arms
//! for the token/behavior deltas. Ported from the iris-agent bench analyzer,
//! but reading the crate's typed [`crate::record::CellRecord`].
//!
//! Discipline carried over from the source analyzer:
//!   - Means are computed ONLY over valid cells (`kind == "real_cell"` and
//!     `valid == true`). `real_cell_error` / `valid == false` rows are counted
//!     as attempted/invalid and excluded from every mean.
//!   - Unparsable / blank lines are tolerated and counted, never fatal.
//!   - Safety: an `approvals == true` or `dangerous_approvals > 0` row in a
//!     no-bash context is a gate-integrity violation and is surfaced. For bash
//!     workloads a dangerous approval is expected and NOT flagged.
//!
//! Public entry points relied on by the rest of the crate:
//!   - `analyze_jsonl(&str) -> Analysis`
//!   - `format_report(&Analysis) -> String`  (plain text)
//!   - `report_from_file(&str) -> io::Result<String>`
//!   - `crate::report::html_report(&Analysis) -> String`

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::record::CellRecord;

/// Per-arm aggregate for one `(model, reasoning, workload)` group. Every mean
/// is over the group's valid cells only; `attempted` counts valid + invalid.
#[derive(Clone, Debug, Default)]
pub struct ArmStats {
    /// Valid + invalid rows routed to this arm.
    pub attempted: usize,
    /// Rows that contributed to the means (valid real cells).
    pub valid: usize,
    /// Valid rows whose `success` was true.
    pub successes: usize,
    pub mean_input_tokens: f64,
    pub mean_tokens_per_turn: f64,
    pub mean_turns: f64,
    pub mean_tool_calls: f64,
    pub mean_tool_result_bytes: f64,
    /// Valid rows with `approvals == true`.
    pub approvals_count: usize,
    /// Sum of `dangerous_approvals` over valid rows.
    pub dangerous_approvals: u64,
    /// Valid rows that took an approval / dangerous approval in a no-bash
    /// context -- gate-integrity violations (normally 0).
    pub safety_violations: usize,
}

impl ArmStats {
    /// Fraction of valid rows that succeeded (0.0 when there are no valid rows).
    pub fn success_rate(&self) -> f64 {
        if self.valid == 0 {
            0.0
        } else {
            self.successes as f64 / self.valid as f64
        }
    }
}

/// One `(model, reasoning, workload)` group with its paired arms and the
/// Defaults-vs-Baseline deltas (populated only when both arms have valid data).
#[derive(Clone, Debug, Default)]
pub struct Group {
    pub model: String,
    pub reasoning: String,
    pub workload: String,
    /// True when any valid row in this group actually used bash (so a dangerous
    /// approval here is expected, not a violation).
    pub is_bash: bool,
    pub defaults: Option<ArmStats>,
    pub baseline: Option<ArmStats>,
    /// Defaults mean input - Baseline mean input (negative = defaults cheaper).
    pub delta_input: f64,
    /// `delta_input` as a percentage of the baseline mean.
    pub delta_input_pct: f64,
    /// Defaults mean tokens/turn - Baseline mean tokens/turn.
    pub delta_tpt: f64,
    pub delta_tpt_pct: f64,
    /// Percent reduction in mean tool_result_bytes (baseline - defaults) /
    /// baseline; positive = defaults emitted fewer bytes.
    pub bytes_reduction_pct: f64,
    /// Safety violations across both arms in this group.
    pub safety_violations: usize,
}

impl Group {
    /// True when both arms produced at least one valid cell (a real pair).
    pub fn is_paired(&self) -> bool {
        matches!(
            (&self.defaults, &self.baseline),
            (Some(d), Some(b)) if d.valid > 0 && b.valid > 0
        )
    }
}

/// Whole-run analysis: parse/skip counts, distinct dimensions, and the paired
/// groups. Field layout is owned by this port.
#[derive(Clone, Debug, Default)]
pub struct Analysis {
    /// Non-blank lines seen.
    pub total_lines: usize,
    /// Lines that deserialized into a `CellRecord`.
    pub parsed: usize,
    /// Blank or unparsable lines (tolerated).
    pub skipped: usize,
    /// Valid real cells that fed the means.
    pub valid_cells: usize,
    /// `real_cell_error` or `valid == false` rows (attempted, excluded).
    pub invalid_cells: usize,
    /// Distinct models seen (sorted).
    pub models: Vec<String>,
    /// Distinct workloads seen (sorted).
    pub workloads: Vec<String>,
    /// Paired groups, sorted by (model, reasoning, workload).
    pub groups: Vec<Group>,
    /// Total safety violations across all groups (normally 0).
    pub safety_violations: usize,
}

/// Internal per-arm accumulator: sums now, means at finalize.
#[derive(Default)]
struct Acc {
    attempted: usize,
    valid: usize,
    successes: usize,
    sum_input: u64,
    sum_tpt: f64,
    sum_turns: u64,
    sum_tool_calls: u64,
    sum_bytes: u64,
    approvals_count: usize,
    dangerous_approvals: u64,
    safety_violations: usize,
}

impl Acc {
    fn finalize(&self) -> ArmStats {
        let n = self.valid.max(1) as f64;
        let has = self.valid > 0;
        ArmStats {
            attempted: self.attempted,
            valid: self.valid,
            successes: self.successes,
            mean_input_tokens: if has { self.sum_input as f64 / n } else { 0.0 },
            mean_tokens_per_turn: if has { self.sum_tpt / n } else { 0.0 },
            mean_turns: if has { self.sum_turns as f64 / n } else { 0.0 },
            mean_tool_calls: if has {
                self.sum_tool_calls as f64 / n
            } else {
                0.0
            },
            mean_tool_result_bytes: if has { self.sum_bytes as f64 / n } else { 0.0 },
            approvals_count: self.approvals_count,
            dangerous_approvals: self.dangerous_approvals,
            safety_violations: self.safety_violations,
        }
    }
}

/// Mutable per-group state while scanning the log.
#[derive(Default)]
struct GroupAcc {
    defaults: Acc,
    baseline: Acc,
    is_bash: bool,
}

/// Did this row exercise bash? Either a recorded exit code or a bash tool call.
fn used_bash(rec: &CellRecord) -> bool {
    !rec.bash_exit_codes.is_empty() || rec.tool_counts.contains_key("bash")
}

/// Parse and aggregate a JSONL run-log body. Blank / unparsable lines are
/// counted as `skipped` and never abort the scan.
pub fn analyze_jsonl(body: &str) -> Analysis {
    let mut groups: BTreeMap<(String, String, String), GroupAcc> = BTreeMap::new();
    let mut models: BTreeSet<String> = BTreeSet::new();
    let mut workloads: BTreeSet<String> = BTreeSet::new();

    let mut total_lines = 0;
    let mut parsed = 0;
    let mut skipped = 0;
    let mut valid_cells = 0;
    let mut invalid_cells = 0;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        total_lines += 1;

        let Ok(rec) = serde_json::from_str::<CellRecord>(trimmed) else {
            skipped += 1;
            continue;
        };
        parsed += 1;

        // Ignore render_probe rows (iris-bench never emits them) and any arm we
        // do not recognize -- neither is a real defaults/baseline cell.
        let arm_defaults = match rec.arm.as_str() {
            "defaults" => true,
            "baseline" => false,
            _ => {
                skipped += 1;
                continue;
            }
        };

        models.insert(rec.model.clone());
        workloads.insert(rec.workload.clone());

        let entry = groups
            .entry((
                rec.model.clone(),
                rec.reasoning.clone(),
                rec.workload.clone(),
            ))
            .or_default();
        let acc = if arm_defaults {
            &mut entry.defaults
        } else {
            &mut entry.baseline
        };
        acc.attempted += 1;

        // Invalid: error rows or explicitly marked invalid. Counted, no means.
        if rec.kind == "real_cell_error" || !rec.valid {
            invalid_cells += 1;
            continue;
        }

        valid_cells += 1;
        acc.valid += 1;
        if rec.success {
            acc.successes += 1;
        }
        acc.sum_input += rec.input_tokens;
        acc.sum_tpt += rec.tokens_per_turn;
        acc.sum_turns += rec.turns as u64;
        acc.sum_tool_calls += rec.tool_calls_total as u64;
        acc.sum_bytes += rec.tool_result_bytes;

        let bash = used_bash(&rec);
        if rec.approvals {
            acc.approvals_count += 1;
        }
        acc.dangerous_approvals += rec.dangerous_approvals as u64;
        // A no-bash cell must never take an approval / dangerous approval.
        if !bash && (rec.approvals || rec.dangerous_approvals > 0) {
            acc.safety_violations += 1;
        }

        // Set last: `acc` is a conditional reborrow of `entry`, so the borrow
        // checker treats it as borrowing all of `entry`. Writing another field
        // is only legal once `acc`'s last use is behind us.
        if bash {
            entry.is_bash = true;
        }
    }

    let mut out_groups = Vec::with_capacity(groups.len());
    let mut safety_violations = 0;
    for ((model, reasoning, workload), g) in groups {
        let group = build_group(model, reasoning, workload, g);
        safety_violations += group.safety_violations;
        out_groups.push(group);
    }

    Analysis {
        total_lines,
        parsed,
        skipped,
        valid_cells,
        invalid_cells,
        models: models.into_iter().collect(),
        workloads: workloads.into_iter().collect(),
        groups: out_groups,
        safety_violations,
    }
}

/// Finalize one group: means per arm and the Defaults-vs-Baseline deltas.
fn build_group(model: String, reasoning: String, workload: String, g: GroupAcc) -> Group {
    let defaults = (g.defaults.attempted > 0).then(|| g.defaults.finalize());
    let baseline = (g.baseline.attempted > 0).then(|| g.baseline.finalize());

    let mut group = Group {
        model,
        reasoning,
        workload,
        is_bash: g.is_bash,
        safety_violations: g.defaults.safety_violations + g.baseline.safety_violations,
        defaults,
        baseline,
        ..Group::default()
    };

    if let (Some(d), Some(b)) = (&group.defaults, &group.baseline)
        && d.valid > 0
        && b.valid > 0
    {
        group.delta_input = d.mean_input_tokens - b.mean_input_tokens;
        group.delta_input_pct = pct_delta(d.mean_input_tokens, b.mean_input_tokens);
        group.delta_tpt = d.mean_tokens_per_turn - b.mean_tokens_per_turn;
        group.delta_tpt_pct = pct_delta(d.mean_tokens_per_turn, b.mean_tokens_per_turn);
        group.bytes_reduction_pct = if b.mean_tool_result_bytes > 0.0 {
            100.0 * (b.mean_tool_result_bytes - d.mean_tool_result_bytes) / b.mean_tool_result_bytes
        } else {
            0.0
        };
    }

    group
}

/// Percent change of `d` relative to `base`, guarding a zero baseline.
fn pct_delta(d: f64, base: f64) -> f64 {
    if base.abs() < f64::EPSILON {
        0.0
    } else {
        100.0 * (d - base) / base
    }
}

/// Render the analysis as a terminal-friendly plain-text report.
pub fn format_report(analysis: &Analysis) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Tokens-per-task analysis");
    let _ = writeln!(out, "========================");
    let _ = writeln!(
        out,
        "Cells: {} valid, {} invalid; lines {} parsed, {} skipped.",
        analysis.valid_cells, analysis.invalid_cells, analysis.parsed, analysis.skipped
    );
    let _ = writeln!(
        out,
        "Dimensions: {} model(s), {} workload(s), {} group(s).",
        analysis.models.len(),
        analysis.workloads.len(),
        analysis.groups.len()
    );
    if analysis.safety_violations == 0 {
        let _ = writeln!(
            out,
            "SAFETY: 0 approval violations in no-bash contexts (clean)."
        );
    } else {
        let _ = writeln!(
            out,
            "SAFETY: {} approval violation(s) in no-bash contexts -- GATE INTEGRITY.",
            analysis.safety_violations
        );
    }
    let _ = writeln!(out);

    if analysis.groups.is_empty() {
        let _ = writeln!(out, "No cells to compare.");
        return out;
    }

    let _ = writeln!(
        out,
        "Per model x reasoning x workload (D = defaults, B = baseline):"
    );
    let _ = writeln!(out);

    let mut cur: Option<(&str, &str)> = None;
    for g in &analysis.groups {
        if cur != Some((g.model.as_str(), g.reasoning.as_str())) {
            cur = Some((g.model.as_str(), g.reasoning.as_str()));
            let reasoning = if g.reasoning.is_empty() {
                "-".to_string()
            } else {
                g.reasoning.clone()
            };
            let _ = writeln!(out, "model {} | reasoning {}", g.model, reasoning);
            let _ = writeln!(
                out,
                "  {:<16} {:<4} {:>7} {:>7} {:>9} {:>8} {:>6} {:>6} {:>10} {:>9}",
                "workload",
                "arm",
                "vld/tot",
                "succ%",
                "in-tok",
                "tok/trn",
                "turns",
                "tools",
                "res-bytes",
                "d-tok%"
            );
        }
        write_group_rows(&mut out, g);
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "d-tok% is the Defaults-vs-Baseline mean input-token delta (negative = defaults cheaper)."
    );
    let _ = writeln!(
        out,
        "res-bytes reduction and tok/turn deltas confirm a reduction fired without lowering success."
    );
    out
}

/// Emit the defaults + baseline rows for one group into the text table.
fn write_group_rows(out: &mut String, g: &Group) {
    let workload = if g.is_bash {
        format!("{} [bash]", g.workload)
    } else {
        g.workload.clone()
    };
    write_arm_row(out, &workload, "D", g.defaults.as_ref(), Some(g), true);
    write_arm_row(out, "", "B", g.baseline.as_ref(), Some(g), false);
}

/// One arm row. The delta column is shown once, on the defaults row.
fn write_arm_row(
    out: &mut String,
    workload: &str,
    arm: &str,
    stats: Option<&ArmStats>,
    group: Option<&Group>,
    show_delta: bool,
) {
    let delta = match (show_delta, group) {
        (true, Some(g)) if g.is_paired() => format!("{:+.1}", g.delta_input_pct),
        _ => "-".to_string(),
    };
    match stats {
        Some(s) => {
            let flag = if s.safety_violations > 0 { " !" } else { "" };
            let _ = writeln!(
                out,
                "  {:<16} {:<4} {:>3}/{:<3} {:>7.0} {:>9.0} {:>8.1} {:>6.1} {:>6.1} {:>10.0} {:>9}{}",
                workload,
                arm,
                s.valid,
                s.attempted,
                s.success_rate() * 100.0,
                s.mean_input_tokens,
                s.mean_tokens_per_turn,
                s.mean_turns,
                s.mean_tool_calls,
                s.mean_tool_result_bytes,
                delta,
                flag,
            );
        }
        None => {
            let _ = writeln!(
                out,
                "  {:<16} {:<4} {:>7} {:>7} {:>9} {:>8} {:>6} {:>6} {:>10} {:>9}",
                workload, arm, "-/-", "-", "-", "-", "-", "-", "-", delta
            );
        }
    }
}

/// Read a JSONL file and produce its text report.
pub fn report_from_file(path: &str) -> std::io::Result<String> {
    let body = std::fs::read_to_string(path)?;
    Ok(format_report(&analyze_jsonl(&body)))
}
