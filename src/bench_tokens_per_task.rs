//! End-to-end tokens-per-completed-task benchmark harness (issue #210,
//! Milestone 2). Proves that Iris's default-on tool-output reductions
//! (grep grouping #338, find grouping #340) lower the prompt tokens spent to
//! COMPLETE a realistic task, without lowering task success.
//!
//! This module is the thin root of a data-driven benchmark package
//! (`src/bench_tokens/`); adding a workload is a table-row change in
//! `workloads.rs`, not new control flow. The pieces:
//! - `arms` -- the Defaults (reductions on) vs Baseline (off) arm; the only
//!   selector is the test-only `ToolState::with_reduce_output(bool)`.
//! - `provider` -- the `ScriptedProvider` replay backend + transcript proxy.
//! - `observer` -- rich per-run instrumentation + the zero-prompt gate.
//! - `fixtures` -- committed-fixture materialization into temp workspaces.
//! - `workloads` -- workload table, scripted sequences, mechanical checks.
//! - `runner` -- the replay + real-provider drivers, records, and JSONL log.
//!
//! Two paths share the fixtures/observer/gate machinery:
//! - **Replay (CI, deterministic, no cost):** `runner::run_replay_arm` replays
//!   a fixed, successful tool-call script per workload. The real built-in tools
//!   run over committed fixtures, so tool OUTPUTS are real; only the assistant's
//!   tool-call CHOICES are scripted. Prompt tokens are an estimated proxy over
//!   the transcript the provider is sent each turn (`bench_support::est_tokens`,
//!   4 bytes/token) -- a ratio, never presented as exact tokens. Asserts, per
//!   workload: (a) the mechanical success check passes in both arms, (b) arm A
//!   (defaults) < arm B (baseline) in proxy tokens by a margin, (c) zero
//!   approval prompts.
//! - **Headline (opt-in, real provider, costs money):** the `#[ignore]`d
//!   `tokens_per_task_headline` test runs the real provider N>=3 times per cell
//!   and reads REAL usage records; gated behind `IRIS_BENCH_REAL=1` so CI never
//!   spends money. See `docs/BENCHMARK_PLAN.md`.
//!
//! Both paths run under the ADR-0032 auto preset with a zero-prompt gate,
//! identical across arms; the safety floors stay active and the agent never
//! calls `bash` (auto-bash is deferred), so a workload that would prompt under
//! auto is a harness/workload bug, caught by the zero-prompt assertion.

#[path = "bench_tokens/analysis.rs"]
mod analysis;
#[path = "bench_tokens/arms.rs"]
mod arms;
#[path = "bench_tokens/fixtures.rs"]
mod fixtures;
#[path = "bench_tokens/observer.rs"]
mod observer;
#[path = "bench_tokens/probes.rs"]
mod probes;
#[path = "bench_tokens/provider.rs"]
mod provider;
#[path = "bench_tokens/runner.rs"]
mod runner;
#[path = "bench_tokens/workloads.rs"]
mod workloads;

// ---------------------------------------------------------------------------
// Replay tests (CI, deterministic)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod replay {
    use super::analysis::{Verdict, analyze_jsonl, format_report, report_from_file};
    use super::arms::Arm;
    use super::probes::{assert_edit_case, assert_render_contract, edit_cases, tool_probes};
    use super::runner::{
        RunMetrics, bench_log_cell_error, bench_log_path, bench_log_render_probe, bench_log_reset,
        bench_reasoning, model_specs, run_real_cell, run_replay_arm, run_scripted_skip_perms,
        selection_for_spec,
    };
    use super::workloads::{
        Workload, bash_workloads, probe_workloads, selected_bash_workloads, selected_workloads,
        workloads,
    };

    /// The margin (in estimated tokens) arm A must beat arm B by, so the win is
    /// not a rounding artifact of the estimator.
    const MIN_MARGIN_TOKENS: usize = 32;

    fn assert_workload(workload: &Workload) -> (RunMetrics, RunMetrics) {
        let arm_a = run_replay_arm(workload, Arm::Defaults);
        let arm_b = run_replay_arm(workload, Arm::Baseline);

        // (c) zero approval prompts in either arm.
        assert!(
            !arm_a.approvals_consulted && !arm_b.approvals_consulted,
            "[{}] approval gate was consulted -- a prompt occurred under auto (run invalid)",
            workload.name
        );
        // (a) success in both arms (both apply the identical fix/answer)...
        assert!(
            arm_a.outcome.success,
            "[{}] arm A failed: {}",
            workload.name, arm_a.outcome.detail
        );
        assert!(
            arm_b.outcome.success,
            "[{}] arm B failed: {}",
            workload.name, arm_b.outcome.detail
        );
        // ...and the reduced (arm A) tool output must still carry every
        // actionable fact the task needs, verbatim -- so success is tied to
        // output fidelity, not just to a scripted answer. Checked in both arms.
        for needle in workload.needles {
            assert!(
                arm_a.transcript.contains(needle),
                "[{}] arm A (reduced) transcript dropped needle {needle:?}",
                workload.name
            );
            assert!(
                arm_b.transcript.contains(needle),
                "[{}] arm B transcript dropped needle {needle:?}",
                workload.name
            );
        }
        // (b) arm A spends fewer prompt tokens than arm B, by a margin.
        assert!(
            arm_a.cumulative_proxy + MIN_MARGIN_TOKENS <= arm_b.cumulative_proxy,
            "[{}] arm A ({}) must beat arm B ({}) by >= {} proxy tokens",
            workload.name,
            arm_a.cumulative_proxy,
            arm_b.cumulative_proxy,
            MIN_MARGIN_TOKENS
        );
        (arm_a, arm_b)
    }

    #[test]
    fn fix_failing_test_arm_a_wins_and_both_succeed() {
        assert_workload(&workloads()[0]);
    }

    #[test]
    fn multi_file_rename_arm_a_wins_and_both_succeed() {
        assert_workload(&workloads()[1]);
    }

    #[test]
    fn investigate_large_log_arm_a_wins_and_both_succeed() {
        assert_workload(&workloads()[2]);
    }

    /// Opt-in real-provider headline (issue #210 DoD #5). Costs money, so it is
    /// `#[ignore]`d AND additionally gated on `IRIS_BENCH_REAL=1`; CI and a plain
    /// `cargo test` never spend money even with `--ignored`. Prints the per-cell
    /// table (workload x arm x run) with REAL usage-record input tokens. N runs
    /// per cell via `IRIS_BENCH_N` (default 3). `IRIS_BENCH_WORKLOAD` (comma-
    /// separated names) narrows to specific workloads (default: all three). Run:
    ///   IRIS_BENCH_REAL=1 cargo test --bin iris tokens_per_task_headline \
    ///     -- --ignored --nocapture
    #[test]
    #[ignore = "real-provider run: costs money; set IRIS_BENCH_REAL=1 to run"]
    fn tokens_per_task_headline() {
        if std::env::var("IRIS_BENCH_REAL").ok().as_deref() != Some("1") {
            eprintln!(
                "skipping real-provider headline: set IRIS_BENCH_REAL=1 (this run costs money)"
            );
            return;
        }
        let n: usize = std::env::var("IRIS_BENCH_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        let specs = model_specs();
        let reasoning = bench_reasoning();
        let cwd = std::env::current_dir().expect("cwd");
        bench_log_reset();
        println!(
            "headline: models={} reasoning={:?} N={} workloads={} log={}",
            specs.join(", "),
            reasoning,
            n,
            selected_workloads().len(),
            bench_log_path()
        );
        println!(
            "| model | workload | arm | run | success | turns | in tok | out tok | tok/turn | tool calls | handles | approvals | note |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        for spec in &specs {
            let selection = match selection_for_spec(&cwd, spec, reasoning) {
                Ok(sel) => sel,
                Err(e) => {
                    bench_log_cell_error(spec, "-", "-", 0, &format!("select: {e}"));
                    println!("| {spec} | - | - | - | - | - | - | - | - | - | - | select: {e} |");
                    continue;
                }
            };
            for workload in selected_workloads() {
                // Baseline first, then defaults -- same order for every cell.
                for arm in [Arm::Baseline, Arm::Defaults] {
                    for run in 0..n {
                        match run_real_cell(spec, &workload, arm, run + 1, &selection) {
                            Ok(m) => println!(
                                "| {} | {} | {} | {} | {} | {} | {} | {} | {:.0} | {} | {} | {} | |",
                                spec,
                                workload.name,
                                m.arm.label(),
                                run + 1,
                                m.outcome.success,
                                m.turns,
                                m.input_tokens,
                                m.output_tokens,
                                m.tokens_per_turn(),
                                m.tool_calls_total(),
                                m.handles_stored,
                                m.approvals_consulted,
                            ),
                            Err(e) => {
                                bench_log_cell_error(spec, workload.name, arm.label(), run + 1, &e);
                                println!(
                                    "| {} | {} | {} | {} | - | - | - | - | - | - | - | - | {} |",
                                    spec,
                                    workload.name,
                                    arm.label(),
                                    run + 1,
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Opt-in real-provider SMOKE (cheapest gate before the headline matrix).
    /// One read-only workload (log triage) x both arms x N=1 per model, over
    /// the `model_specs()` matrix, with reasoning forced by `bench_reasoning()`
    /// (default low). Reports per-model REACHABILITY (a backend reject is a
    /// recorded row, not a panic), so we learn which model ids the current
    /// OAuth actually serves -- and whether Haiku accepts a thinking level --
    /// for a handful of calls before committing to N=3. Costs money; `#[ignore]`d
    /// and gated on `IRIS_BENCH_REAL=1`. Run:
    ///   IRIS_BENCH_REAL=1 cargo test --bin iris tokens_per_task_smoke \
    ///     -- --ignored --nocapture
    #[test]
    #[ignore = "real-provider smoke: costs a few calls; set IRIS_BENCH_REAL=1 to run"]
    fn tokens_per_task_smoke() {
        if std::env::var("IRIS_BENCH_REAL").ok().as_deref() != Some("1") {
            eprintln!("skipping real-provider smoke: set IRIS_BENCH_REAL=1 (this run costs money)");
            return;
        }
        let specs = model_specs();
        let reasoning = bench_reasoning();
        let workload = &workloads()[2]; // log triage: 3 turns, read/grep only.
        let cwd = std::env::current_dir().expect("cwd");
        bench_log_reset();
        println!(
            "smoke: workload={} reasoning={:?} models={} log={}",
            workload.name,
            reasoning,
            specs.join(", "),
            bench_log_path()
        );
        println!(
            "| model | arm | reachable | success | turns | in tok | out tok | tool calls | approvals | note |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|");
        let mut reachable_with_approval = false;
        for spec in &specs {
            let selection = match selection_for_spec(&cwd, spec, reasoning) {
                Ok(sel) => sel,
                Err(e) => {
                    bench_log_cell_error(spec, workload.name, "-", 0, &format!("select: {e}"));
                    println!("| {spec} | - | no | - | - | - | - | - | select: {e} |");
                    continue;
                }
            };
            for arm in [Arm::Baseline, Arm::Defaults] {
                match run_real_cell(spec, workload, arm, 1, &selection) {
                    Ok(m) => {
                        if m.approvals_consulted {
                            reachable_with_approval = true;
                        }
                        println!(
                            "| {} | {} | yes | {} | {} | {} | {} | {} | {} | |",
                            spec,
                            m.arm.label(),
                            m.outcome.success,
                            m.turns,
                            m.input_tokens,
                            m.output_tokens,
                            m.tool_calls_total(),
                            m.approvals_consulted,
                        );
                    }
                    Err(e) => {
                        bench_log_cell_error(spec, workload.name, arm.label(), 1, &e);
                        println!(
                            "| {} | {} | no | - | - | - | - | - | {} |",
                            spec,
                            arm.label(),
                            e
                        );
                    }
                }
            }
        }
        // A reachable run that consulted the approval gate means the workload
        // would prompt under auto for that model -- the run is invalid and the
        // workload/prompt must be fixed before the headline matrix.
        assert!(
            !reachable_with_approval,
            "a reachable smoke run consulted the approval gate (a prompt occurred under auto); \
             fix the workload prompt before running the headline"
        );
    }

    #[test]
    fn tokens_per_task_replay_report() {
        // Prints the deterministic replay table committed to
        // docs/benchmarks/tokens-per-task.md (run with --nocapture).
        println!(
            "| workload | arm | success | turns | cumulative proxy tokens | final context proxy | reduction |"
        );
        println!("|---|---|---|---|---|---|---|");
        for workload in workloads() {
            let a = run_replay_arm(&workload, Arm::Defaults);
            let b = run_replay_arm(&workload, Arm::Baseline);
            let reduction = if b.cumulative_proxy == 0 {
                0.0
            } else {
                100.0 * (1.0 - a.cumulative_proxy as f64 / b.cumulative_proxy as f64)
            };
            for m in [&b, &a] {
                println!(
                    "| {} | {} | {} | {} | {} | {} | {} |",
                    workload.name,
                    m.arm.label(),
                    m.outcome.success,
                    m.provider_turns,
                    m.cumulative_proxy,
                    m.final_context_proxy,
                    if m.arm == Arm::Defaults {
                        format!("{reduction:.1}%")
                    } else {
                        "-".to_string()
                    },
                );
            }
        }
    }

    /// Deterministic proof that `--dangerously-skip-permissions` (ADR-0049)
    /// unlocks `bash` in the harness: the scripted bash call runs, the deny
    /// gate is bypassed (never consulted), the dangerous auto-approval fires,
    /// and the non-zero exit code + result bytes are captured. No real
    /// provider -- runs in CI, in both arms.
    #[test]
    fn bash_wiring_skip_permissions() {
        let workload = &bash_workloads()[0];
        for arm in [Arm::Baseline, Arm::Defaults] {
            let run = run_scripted_skip_perms(workload, arm);
            assert!(
                !run.approvals_consulted,
                "[{}] deny gate was consulted -- skip-permissions did not bypass it first",
                arm.label()
            );
            assert!(
                run.dangerous_approvals >= 1,
                "[{}] expected a ToolAutoApprovedDangerous event (bash under skip-perms)",
                arm.label()
            );
            assert_eq!(
                run.bash_exit_codes,
                vec![3],
                "[{}] bash must execute and report its exit code (3)",
                arm.label()
            );
            assert!(
                run.tool_result_bytes > 0,
                "[{}] bash result bytes must enter context",
                arm.label()
            );
            // The scripted answer carries the planted values, so the mechanical
            // check passes -- this proves the check wiring, not output quality
            // (that is the live smoke's job).
            assert!(
                run.outcome.success,
                "[{}] scripted answer should satisfy the diagnosis check: {}",
                arm.label(),
                run.outcome.detail
            );
        }
    }

    /// Deterministic chained bash workload from PR #404: the scripted agent has
    /// to discover provider files, inspect a noisy failing cargo test, edit the
    /// request builder, and verify green. This proves the workload wiring before
    /// any real-provider run and checks the reduced arm materially shrinks tool
    /// output while still solving the task.
    #[test]
    fn chained_bash_workload_replay_fixes_provider_and_saves_tokens() {
        let workload = &bash_workloads()[1];
        let baseline = run_scripted_skip_perms(workload, Arm::Baseline);
        let defaults = run_scripted_skip_perms(workload, Arm::Defaults);
        for (arm, run) in [(Arm::Baseline, &baseline), (Arm::Defaults, &defaults)] {
            assert!(
                !run.approvals_consulted,
                "[{}] deny gate was consulted -- skip-permissions did not bypass it first",
                arm.label()
            );
            assert!(
                run.dangerous_approvals >= 2,
                "[{}] expected both cargo-test bash calls to be auto-approved dangerous",
                arm.label()
            );
            assert_eq!(
                run.bash_exit_codes,
                vec![101, 0],
                "[{}] first cargo test should fail, second should pass",
                arm.label()
            );
            assert!(
                run.outcome.success,
                "[{}] scripted repair should satisfy the mechanical check: {}",
                arm.label(),
                run.outcome.detail
            );
        }
        assert!(
            defaults.tool_result_bytes < baseline.tool_result_bytes,
            "reduced arm should shrink chained find/grep/bash output (A={} B={})",
            defaults.tool_result_bytes,
            baseline.tool_result_bytes
        );
    }

    /// Deterministic per-tool RENDER PROBE (Phase 5, layer 1), FAST set. For
    /// each non-slow tool probe, invoke the tool over its workspace in both arms
    /// and prove the reduced output (a) clears its token-reduction bar and (b)
    /// keeps every needle verbatim. No real provider -- runs in CI. This is the
    /// "reduction is real and lossless for the asked fact" half the paired live
    /// probe builds on.
    #[test]
    fn tool_render_probes_reduce_and_preserve() {
        for probe in tool_probes().iter().filter(|p| !p.slow) {
            let r = assert_render_contract(probe);
            println!(
                "[{}] reduction {:.1}% (baseline {} B -> reduced {} B); needles survived",
                probe.name,
                r.reduction_pct,
                r.baseline.len(),
                r.reduced.len()
            );
        }
    }

    /// SLOW render probes (compile / heavy spawn), opt-in. Same contract as the
    /// fast set, kept out of the CI gate for speed. Run:
    ///   cargo test --bin iris tool_render_probes_slow -- --ignored --nocapture
    /// The edit result-class probe (issue-341): every outcome class edit
    /// distinguishes holds -- correct class token, success flag, and on-disk
    /// effect -- and an exact success stays terser than a tolerant success
    /// (the ADR-0038 conditional echo fires only on a tolerant match).
    /// Deterministic; no provider.
    #[test]
    fn edit_result_classes_hold() {
        let mut exact_len = None;
        let mut tolerant_len = None;
        for case in edit_cases() {
            let outcome = assert_edit_case(&case);
            match case.name {
                "exact" => exact_len = Some(outcome.output_len),
                "tolerant" => tolerant_len = Some(outcome.output_len),
                _ => {}
            }
        }
        let exact_len = exact_len.expect("exact case present");
        let tolerant_len = tolerant_len.expect("tolerant case present");
        assert!(
            exact_len < tolerant_len,
            "exact success ({exact_len} B) must stay terser than tolerant success \
             ({tolerant_len} B); the ADR-0038 conditional echo fires only on tolerant",
        );
    }

    /// Build one synthetic `real_cell` JSONL line for the analyzer verdict test.
    #[cfg(test)]
    fn cell_line(model: &str, wl: &str, reduce: bool, ok: bool, turns: u64, input: u64) -> String {
        serde_json::json!({
            "kind": "real_cell", "valid": true, "schema_version": 3,
            "model": model, "workload": wl, "reduce_output": reduce,
            "success": ok, "turns": turns, "input_tokens": input,
            "tool_result_bytes": if reduce { 400 } else { 500 },
            "tool_calls_total": turns + 1,
            "tool_errors": [],
        })
        .to_string()
    }

    /// The Phase 7 analyzer's honesty verdicts hold on synthetic logs -- one
    /// case per branch (Supported / SuccessRegression / BaselineWins /
    /// Inconclusive) plus a render probe, an error cell, an invalid (usage-None)
    /// cell, and a garbage line. Deterministic; no provider.
    #[test]
    fn analyzer_verdicts_hold() {
        let mut lines: Vec<String> = Vec::new();
        // Supported: A cheaper, success held, spreads separated, N=5.
        for i in 0..5 {
            lines.push(cell_line("m1", "w-supported", false, true, 3, 1000 + i));
            lines.push(cell_line("m1", "w-supported", true, true, 3, 900 + i));
        }
        // SuccessRegression: reduced arm drops 2 of 5 successes.
        for i in 0..5 {
            lines.push(cell_line("m1", "w-regress", false, true, 3, 1000));
            lines.push(cell_line("m1", "w-regress", true, i < 3, 3, 900));
        }
        // BaselineWins: reduced arm costs MORE.
        for _ in 0..5 {
            lines.push(cell_line("m1", "w-baseline", false, true, 3, 1000));
            lines.push(cell_line("m1", "w-baseline", true, true, 3, 1100));
        }
        // Inconclusive: A cheaper on median but the saving CI crosses zero.
        for i in 0..5 {
            lines.push(cell_line("m1", "w-incon", false, true, 3, 1000 + i * 3));
            lines.push(cell_line("m1", "w-incon", true, true, 3, 995 + i * 3));
        }
        // Supported despite OVERLAPPING RANGES: means clearly separated, tight
        // variance, N=10. The old max_A>=min_B guard would have called this
        // INCONCLUSIVE; the Welch CI correctly certifies it (the N=50 Sonnet
        // situation in miniature).
        for i in 0..10 {
            lines.push(cell_line("m1", "w-overlap-sig", false, true, 3, 1000 + i));
            lines.push(cell_line("m1", "w-overlap-sig", true, true, 3, 995 + i));
        }
        // Inconclusive at LARGE N: A cheaper on median but high variance -> the
        // saving CI still spans zero (the N=50 GPT-5.4 situation). Proves N alone
        // never buys a claim.
        let noisy_b = [500, 700, 900, 1100, 1300, 600, 800, 1000, 1200, 1400];
        for b in noisy_b {
            lines.push(cell_line("m1", "w-noisy", false, true, 3, b));
            lines.push(cell_line("m1", "w-noisy", true, true, 3, b - 50));
        }
        // Coverage: a render probe, an error cell, an invalid (usage-None) cell,
        // and an unparsable line -- all counted, none crash the analyzer.
        lines.push(
            serde_json::json!({ "kind": "render_probe", "probe": "grep-x", "tool": "grep",
                "reduction_pct": 36.4, "needles_survived": true,
                "baseline_proxy_tokens": 1606, "reduced_proxy_tokens": 1021 })
            .to_string(),
        );
        lines.push(
            serde_json::json!({ "kind": "real_cell_error", "valid": false, "model": "m1",
                "workload": "w-supported", "arm": "-", "error": "select: unreachable" })
            .to_string(),
        );
        lines.push(cell_line("m1", "w-supported", true, true, 3, 0)); // usage None => invalid
        lines.push("{ not json".to_string());

        let analysis = analyze_jsonl(&lines.join("\n"));

        let verdict = |wl: &str| {
            analysis
                .pairings
                .iter()
                .find(|p| p.workload == wl)
                .unwrap_or_else(|| panic!("missing pairing {wl}"))
                .verdict
        };
        assert_eq!(verdict("w-supported"), Verdict::Supported);
        assert_eq!(verdict("w-regress"), Verdict::SuccessRegression);
        assert_eq!(verdict("w-baseline"), Verdict::BaselineWins);
        assert_eq!(verdict("w-incon"), Verdict::Inconclusive);
        // The Welch-CI upgrade: overlapping ranges but separated means -> Supported;
        // large N but high variance -> still Inconclusive.
        assert_eq!(verdict("w-overlap-sig"), Verdict::Supported);
        assert_eq!(verdict("w-noisy"), Verdict::Inconclusive);
        // Overall is the most-blocking pairing verdict.
        assert_eq!(analysis.overall, Verdict::SuccessRegression);
        assert_eq!(analysis.cell_count, 80);
        assert_eq!(analysis.invalid_count, 1);
        assert_eq!(analysis.error_count, 1);
        assert!(analysis.skipped_lines >= 1);
        assert_eq!(analysis.render_rows.len(), 1);
        // The decomposition reconciles: at equal turns, all of the delta is the
        // efficiency term and the turn term is zero.
        let sup = analysis
            .pairings
            .iter()
            .find(|p| p.workload == "w-supported")
            .unwrap();
        assert!(sup.delta_input < 0, "defaults should be cheaper");
        assert_eq!(
            sup.term_turns as i64, 0,
            "equal turns => no turn-count term"
        );

        let report = format_report(&analysis);
        assert!(report.contains("OVERALL VERDICT: SUCCESS REGRESSION"));
        assert!(report.contains("## Safety / loop signals"));
        assert!(report.contains("tool calls med a/b"));
        assert!(report.contains("w-baseline"));
    }

    /// Opt-in: analyze a real run's JSONL (default `IRIS_BENCH_LOG`) and print
    /// the Markdown report. Run AFTER an authorized live matrix:
    ///   IRIS_BENCH_LOG=... cargo test --bin iris tokens_per_task_report -- --ignored --nocapture
    #[test]
    #[ignore = "reads a real run's JSONL; run after an authorized matrix"]
    fn tokens_per_task_report() {
        let path = bench_log_path();
        match report_from_file(&path) {
            Ok(report) => {
                println!("{report}");
                println!("(analyzed {path})");
            }
            Err(e) => println!("no readable log at {path}: {e}"),
        }
    }

    #[test]
    #[ignore = "slow render probe (compiles a fixture crate); run on demand"]
    fn tool_render_probes_slow() {
        for probe in tool_probes().iter().filter(|p| p.slow) {
            let r = assert_render_contract(probe);
            println!(
                "[{}] reduction {:.1}% (baseline {} B -> reduced {} B); needles survived",
                probe.name,
                r.reduction_pct,
                r.baseline.len(),
                r.reduced.len()
            );
        }
    }

    /// Log ALL render-probe measurements (fast + slow) to the JSONL as
    /// `kind:"render_probe"` records, so the analyzer can correlate a tool's
    /// render reduction with its live outcome. Opt-in (writes the log file and
    /// compiles the slow probe), so it is not in the CI gate. Run:
    ///   cargo test --bin iris tool_render_probe_log -- --ignored --nocapture
    #[test]
    #[ignore = "writes JSONL + compiles slow probe; run on demand"]
    fn tool_render_probe_log() {
        bench_log_reset();
        for probe in tool_probes() {
            // assert_render_contract panics unless every needle survived, so
            // reaching the log line means needles_survived is true.
            let r = assert_render_contract(&probe);
            bench_log_render_probe(
                probe.name,
                probe.tool,
                &r.baseline,
                &r.reduced,
                r.reduction_pct,
                true,
            );
            println!("logged [{}] {:.1}%", probe.name, r.reduction_pct);
        }
        println!("render-probe log -> {}", bench_log_path());
    }

    /// Opt-in real-provider BASH matrix (Phase 4+). Runs bash-enabled workloads
    /// under `--dangerously-skip-permissions` so the real model can run
    /// `cargo test`/build-test loops. Double-gated: `IRIS_BENCH_REAL=1` AND
    /// `IRIS_BENCH_DANGEROUS_OK=1` (it executes shell commands). `IRIS_BENCH_N`
    /// controls runs per arm; `IRIS_BENCH_WORKLOAD` can select a specific bash
    /// workload such as `chained-openai-summary-fix`. Asserts the deny gate is
    /// never consulted (the bypass fired). Run:
    ///   IRIS_BENCH_REAL=1 IRIS_BENCH_DANGEROUS_OK=1 cargo test --bin iris \
    ///     tokens_per_task_bash_smoke -- --ignored --nocapture
    #[test]
    #[ignore = "real-provider bash smoke: costs calls and runs bash; set IRIS_BENCH_REAL=1 and IRIS_BENCH_DANGEROUS_OK=1"]
    fn tokens_per_task_bash_smoke() {
        if std::env::var("IRIS_BENCH_REAL").ok().as_deref() != Some("1") {
            eprintln!("skipping bash smoke: set IRIS_BENCH_REAL=1 (this run costs money)");
            return;
        }
        if std::env::var("IRIS_BENCH_DANGEROUS_OK").ok().as_deref() != Some("1") {
            eprintln!(
                "skipping bash smoke: set IRIS_BENCH_DANGEROUS_OK=1 (this run executes bash under --dangerously-skip-permissions)"
            );
            return;
        }
        let n: usize = std::env::var("IRIS_BENCH_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let specs = model_specs();
        let reasoning = bench_reasoning();
        let workloads = selected_bash_workloads();
        let cwd = std::env::current_dir().expect("cwd");
        bench_log_reset();
        println!(
            "bash smoke: workloads={} reasoning={:?} N={} models={} log={}",
            workloads.len(),
            reasoning,
            n,
            specs.join(", "),
            bench_log_path()
        );
        println!(
            "| workload | model | arm | run | reachable | success | turns | in tok | bash exits | dangerous | approvals | note |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
        let mut consulted_any = false;
        for workload in &workloads {
            for spec in &specs {
                let selection = match selection_for_spec(&cwd, spec, reasoning) {
                    Ok(sel) => sel,
                    Err(e) => {
                        bench_log_cell_error(spec, workload.name, "-", 0, &format!("select: {e}"));
                        println!(
                            "| {} | {spec} | - | - | no | - | - | - | - | - | - | select: {e} |",
                            workload.name
                        );
                        continue;
                    }
                };
                for arm in [Arm::Baseline, Arm::Defaults] {
                    for run in 0..n {
                        match run_real_cell(spec, workload, arm, run + 1, &selection) {
                            Ok(m) => {
                                if m.approvals_consulted {
                                    consulted_any = true;
                                }
                                println!(
                                    "| {} | {} | {} | {} | yes | {} | {} | {} | {:?} | {} | {} | |",
                                    workload.name,
                                    spec,
                                    m.arm.label(),
                                    run + 1,
                                    m.outcome.success,
                                    m.turns,
                                    m.input_tokens,
                                    m.bash_exit_codes,
                                    m.dangerous_approvals,
                                    m.approvals_consulted,
                                );
                            }
                            Err(e) => {
                                bench_log_cell_error(spec, workload.name, arm.label(), run + 1, &e);
                                println!(
                                    "| {} | {} | {} | {} | no | - | - | - | - | - | - | {} |",
                                    workload.name,
                                    spec,
                                    arm.label(),
                                    run + 1,
                                    e
                                )
                            }
                        }
                    }
                }
            }
        }
        assert!(
            !consulted_any,
            "a bash smoke run consulted the deny gate; skip-permissions must bypass it"
        );
    }

    /// Opt-in real-provider MICRO-PROBE (Phase 5, layer 2). For each per-tool
    /// probe workload, a real model must answer an EXACT question from one
    /// tool's (reduced) output; scored mechanically. Runs both arms per model
    /// so the table shows whether the reduced output still lets the model
    /// answer AND what it costs -- paired with the deterministic render probe
    /// (`tool_render_probes_reduce_and_preserve`). Behavior metrics (grep/read
    /// call counts, turns, tok/turn) come from the JSONL schema. Run:
    ///   IRIS_BENCH_REAL=1 cargo test --bin iris tokens_per_task_micro_probes \
    ///     -- --ignored --nocapture
    #[test]
    #[ignore = "real-provider micro-probe: costs calls; set IRIS_BENCH_REAL=1"]
    fn tokens_per_task_micro_probes() {
        if std::env::var("IRIS_BENCH_REAL").ok().as_deref() != Some("1") {
            eprintln!("skipping micro-probes: set IRIS_BENCH_REAL=1 (this run costs money)");
            return;
        }
        let specs = model_specs();
        let reasoning = bench_reasoning();
        let cwd = std::env::current_dir().expect("cwd");
        bench_log_reset();
        println!(
            "micro-probes: reasoning={:?} models={} log={}",
            reasoning,
            specs.join(", "),
            bench_log_path()
        );
        println!(
            "| probe | model | arm | success | turns | in tok | tok/turn | grep | read | approvals | note |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|---|");
        for workload in probe_workloads() {
            for spec in &specs {
                let selection = match selection_for_spec(&cwd, spec, reasoning) {
                    Ok(sel) => sel,
                    Err(e) => {
                        bench_log_cell_error(spec, workload.name, "-", 0, &format!("select: {e}"));
                        println!(
                            "| {} | {} | - | - | - | - | - | - | - | - | select: {e} |",
                            workload.name, spec
                        );
                        continue;
                    }
                };
                for arm in [Arm::Baseline, Arm::Defaults] {
                    match run_real_cell(spec, &workload, arm, 1, &selection) {
                        Ok(m) => {
                            let grep = m.tool_counts.get("grep").copied().unwrap_or(0);
                            let read = m.tool_counts.get("read").copied().unwrap_or(0);
                            println!(
                                "| {} | {} | {} | {} | {} | {} | {:.0} | {} | {} | {} | |",
                                workload.name,
                                spec,
                                m.arm.label(),
                                m.outcome.success,
                                m.turns,
                                m.input_tokens,
                                m.tokens_per_turn(),
                                grep,
                                read,
                                m.approvals_consulted,
                            );
                        }
                        Err(e) => {
                            bench_log_cell_error(spec, workload.name, arm.label(), 1, &e);
                            println!(
                                "| {} | {} | {} | - | - | - | - | - | - | - | {} |",
                                workload.name,
                                spec,
                                arm.label(),
                                e
                            );
                        }
                    }
                }
            }
        }
    }
}
