//! Static HTML report generation from an [`Analysis`]. Self-contained: all data
//! and styling are inlined so the file opens in a browser with no server, no
//! external CSS/JS/fonts, and no added dependencies (std only + the crate's
//! analysis types).

use std::fmt::Write as _;

use crate::analysis::{Analysis, ArmStats, Group};

/// Minimal HTML-attribute/text escaper. Covers the five characters that can
/// break out of element text or attribute context; all dynamic strings
/// (workloads, model/reasoning names) pass through here.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

const STYLE: &str = "\
body{font-family:-apple-system,Segoe UI,Roboto,Helvetica,Arial,monospace;\
margin:2rem;color:#1a1a1a;background:#fafafa}\
h1{font-size:1.4rem;margin:0 0 .25rem}\
h2{font-size:1.05rem;margin:1.75rem 0 .5rem;border-bottom:1px solid #ccc;padding-bottom:.2rem}\
.summary{color:#333;margin:.25rem 0 1rem}\
.safe{color:#0a7d28;font-weight:600}\
.unsafe{color:#b00020;font-weight:700}\
table{border-collapse:collapse;width:100%;margin:.25rem 0 1rem;font-size:.85rem}\
th,td{border:1px solid #ddd;padding:.3rem .5rem;text-align:right;white-space:nowrap}\
th{background:#eee;text-align:right}\
td.l,th.l{text-align:left}\
tr.defaults td{background:#f2f7ff}\
td.viol{background:#ffd6dc;color:#b00020;font-weight:700}\
td.good{color:#0a7d28}\
td.bad{color:#b00020}\
.bash{color:#555;font-size:.8rem}";

/// Render a self-contained HTML metrics report for the whole run.
pub fn html_report(analysis: &Analysis) -> String {
    let mut out = String::new();
    out.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    out.push_str("<title>iris-bench report</title><style>");
    out.push_str(STYLE);
    out.push_str("</style></head><body>");

    out.push_str("<h1>iris-bench tokens-per-task report</h1>");
    let _ = write!(
        out,
        "<p class=\"summary\">{} model(s), {} workload(s), {} group(s) &middot; \
         {} valid cell(s), {} invalid &middot; {} line(s) parsed, {} skipped</p>",
        analysis.models.len(),
        analysis.workloads.len(),
        analysis.groups.len(),
        analysis.valid_cells,
        analysis.invalid_cells,
        analysis.parsed,
        analysis.skipped,
    );

    if analysis.safety_violations == 0 {
        out.push_str("<p class=\"safe\">SAFETY: 0 approval violations in no-bash contexts.</p>");
    } else {
        let _ = write!(
            out,
            "<p class=\"unsafe\">SAFETY: {} approval violation(s) in no-bash contexts \
             -- gate integrity broken.</p>",
            analysis.safety_violations
        );
    }

    if analysis.groups.is_empty() {
        out.push_str("<p>No cells to compare.</p></body></html>\n");
        return out;
    }

    // Groups arrive sorted by (model, reasoning, workload); one table per
    // (model, reasoning) block.
    let mut cur: Option<(&str, &str)> = None;
    for g in &analysis.groups {
        let key = (g.model.as_str(), g.reasoning.as_str());
        if cur != Some(key) {
            if cur.is_some() {
                out.push_str("</tbody></table>");
            }
            cur = Some(key);
            open_table(&mut out, g);
        }
        write_group(&mut out, g);
    }
    out.push_str("</tbody></table>");

    out.push_str("</body></html>\n");
    out
}

/// Open a `(model, reasoning)` section heading and table header.
fn open_table(out: &mut String, g: &Group) {
    let reasoning = if g.reasoning.is_empty() {
        "-".to_string()
    } else {
        esc(&g.reasoning)
    };
    let _ = write!(
        out,
        "<h2>model {} &middot; reasoning {}</h2>",
        esc(&g.model),
        reasoning
    );
    out.push_str("<table><thead><tr>");
    for (label, left) in [
        ("workload", true),
        ("arm", true),
        ("valid/total", false),
        ("success%", false),
        ("mean input tok", false),
        ("tok/turn", false),
        ("turns", false),
        ("tool calls", false),
        ("tool_result bytes", false),
        ("D-vs-B tok delta%", false),
    ] {
        let cls = if left { " class=\"l\"" } else { "" };
        let _ = write!(out, "<th{cls}>{label}</th>");
    }
    out.push_str("</tr></thead><tbody>");
}

/// Emit the defaults + baseline rows for one group.
fn write_group(out: &mut String, g: &Group) {
    let workload = esc(&g.workload);
    let workload_cell = if g.is_bash {
        format!("{workload} <span class=\"bash\">[bash]</span>")
    } else {
        workload
    };
    let delta = if g.is_paired() {
        let cls = if g.delta_input_pct < 0.0 {
            "good"
        } else {
            "bad"
        };
        format!("<td class=\"{cls}\">{:+.1}%</td>", g.delta_input_pct)
    } else {
        "<td>-</td>".to_string()
    };

    write_arm(out, &workload_cell, "defaults", g.defaults.as_ref(), &delta);
    write_arm(out, "", "baseline", g.baseline.as_ref(), "<td>-</td>");
}

/// One arm row; `workload_cell` is pre-escaped HTML, `delta_cell` a full `<td>`.
fn write_arm(
    out: &mut String,
    workload_cell: &str,
    arm: &str,
    stats: Option<&ArmStats>,
    delta_cell: &str,
) {
    let row_cls = if arm == "defaults" {
        " class=\"defaults\""
    } else {
        ""
    };
    let _ = write!(out, "<tr{row_cls}>");
    let _ = write!(out, "<td class=\"l\">{workload_cell}</td>");
    let _ = write!(out, "<td class=\"l\">{arm}</td>");

    match stats {
        Some(s) => {
            let vt_cls = if s.safety_violations > 0 {
                " class=\"viol\""
            } else {
                ""
            };
            let _ = write!(out, "<td{vt_cls}>{}/{}", s.valid, s.attempted);
            if s.safety_violations > 0 {
                out.push_str(" !");
            }
            out.push_str("</td>");
            let _ = write!(out, "<td>{:.0}%</td>", s.success_rate() * 100.0);
            let _ = write!(out, "<td>{:.0}</td>", s.mean_input_tokens);
            let _ = write!(out, "<td>{:.0}</td>", s.mean_tokens_per_turn);
            let _ = write!(out, "<td>{:.1}</td>", s.mean_turns);
            let _ = write!(out, "<td>{:.1}</td>", s.mean_tool_calls);
            let _ = write!(out, "<td>{:.0}</td>", s.mean_tool_result_bytes);
        }
        None => {
            out.push_str(
                "<td>-/-</td><td>-</td><td>-</td><td>-</td><td>-</td><td>-</td><td>-</td>",
            );
        }
    }
    out.push_str(delta_cell);
    out.push_str("</tr>");
}
