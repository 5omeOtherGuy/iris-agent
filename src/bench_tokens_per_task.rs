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
    use super::arms::Arm;
    use super::probes::{assert_render_contract, tool_probes};
    use super::runner::{
        RunMetrics, bench_log_path, bench_log_reset, bench_reasoning, model_specs, run_real_cell,
        run_replay_arm, run_scripted_skip_perms, selection_for_spec,
    };
    use super::workloads::{Workload, bash_workloads, probe_workloads, workloads};

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
    /// per cell via `IRIS_BENCH_N` (default 3). Run:
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
            workloads().len(),
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
                    println!("| {spec} | - | - | - | - | - | - | - | - | - | - | select: {e} |");
                    continue;
                }
            };
            for workload in workloads() {
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
                            Err(e) => println!(
                                "| {} | {} | {} | {} | - | - | - | - | - | - | - | - | {} |",
                                spec,
                                workload.name,
                                arm.label(),
                                run + 1,
                                e
                            ),
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

    /// Deterministic per-tool RENDER PROBE (Phase 5, layer 1). For each tool
    /// probe, invoke the tool over its fixture in both arms and prove the
    /// reduced output (a) clears its token-reduction bar and (b) keeps every
    /// needle verbatim. No real provider -- runs in CI. This is the "the
    /// reduction is real and lossless for the asked fact" half that the paired
    /// live probe builds on.
    #[test]
    fn tool_render_probes_reduce_and_preserve() {
        for probe in tool_probes() {
            let r = assert_render_contract(&probe);
            println!(
                "[{}] reduction {:.1}% (baseline {} B -> reduced {} B); needles survived",
                probe.name,
                r.reduction_pct,
                r.baseline.len(),
                r.reduced.len()
            );
        }
    }

    /// Opt-in real-provider BASH smoke (Phase 4). Runs the read-only diagnosis
    /// workload under `--dangerously-skip-permissions` so the real model runs
    /// `bash` (e.g. `cargo test`) to find the failure, over the model matrix x
    /// both arms x N=1. Double-gated: `IRIS_BENCH_REAL=1` AND
    /// `IRIS_BENCH_DANGEROUS_OK=1` (it executes shell commands). Asserts the
    /// deny gate is never consulted (the bypass fired). Run:
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
        let specs = model_specs();
        let reasoning = bench_reasoning();
        let workload = &bash_workloads()[0];
        let cwd = std::env::current_dir().expect("cwd");
        bench_log_reset();
        println!(
            "bash smoke: workload={} reasoning={:?} models={} log={}",
            workload.name,
            reasoning,
            specs.join(", "),
            bench_log_path()
        );
        println!(
            "| model | arm | reachable | success | turns | in tok | bash exits | dangerous | approvals | note |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|");
        let mut consulted_any = false;
        for spec in &specs {
            let selection = match selection_for_spec(&cwd, spec, reasoning) {
                Ok(sel) => sel,
                Err(e) => {
                    println!("| {spec} | - | no | - | - | - | - | - | - | select: {e} |");
                    continue;
                }
            };
            for arm in [Arm::Baseline, Arm::Defaults] {
                match run_real_cell(spec, workload, arm, 1, &selection) {
                    Ok(m) => {
                        if m.approvals_consulted {
                            consulted_any = true;
                        }
                        println!(
                            "| {} | {} | yes | {} | {} | {} | {:?} | {} | {} | |",
                            spec,
                            m.arm.label(),
                            m.outcome.success,
                            m.turns,
                            m.input_tokens,
                            m.bash_exit_codes,
                            m.dangerous_approvals,
                            m.approvals_consulted,
                        );
                    }
                    Err(e) => {
                        println!(
                            "| {} | {} | no | - | - | - | - | - | - | {} |",
                            spec,
                            arm.label(),
                            e
                        )
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
                        Err(e) => println!(
                            "| {} | {} | {} | - | - | - | - | - | - | - | {} |",
                            workload.name,
                            spec,
                            arm.label(),
                            e
                        ),
                    }
                }
            }
        }
    }
}
