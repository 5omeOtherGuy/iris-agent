//! Benchmark and quality-loss corpus for the output filter (ADR-0037).
//!
//! Fixtures under `corpus/` are captured outputs of real command runs (cargo,
//! git, jest via npm, npm install, shellcheck). Three test classes enforce the
//! ADR-0036 acceptance bar:
//! - noisy classes (test runs, install logs) must lose >= 60% of estimated
//!   tokens;
//! - failure samples must keep error messages, file:line references, and
//!   failing test names verbatim (zero quality loss);
//! - per-sample filter overhead must stay under 10 ms.
//!
//! `corpus_benchmark_report` prints the per-class table behind the committed
//! report in `docs/benchmarks/adr-0037-bash-filter-tokens.md`; regenerate with
//! `cargo test corpus_benchmark_report -- --nocapture`.

use super::filter_output;

struct Sample {
    /// Command class for the report.
    class: &'static str,
    /// Command string as the model would send it (drives dispatch).
    command: &'static str,
    /// Captured raw output.
    raw: &'static str,
    /// Whether the captured run exited 0.
    exit_ok: bool,
    /// Required minimum token reduction (percent) -- noisy classes only.
    min_reduction: Option<u32>,
    /// Content that must survive filtering verbatim.
    must_contain: &'static [&'static str],
    /// True when no PR-1 filter covers the class (structured filters are
    /// PR 2): the output must pass through untouched.
    expect_passthrough: bool,
}

fn samples() -> Vec<Sample> {
    vec![
        Sample {
            class: "cargo test (pass)",
            command: "cargo test",
            raw: include_str!("corpus/cargo-test-pass.txt"),
            exit_ok: true,
            min_reduction: Some(60),
            must_contain: &["test result: ok. 48 passed; 0 failed"],
            expect_passthrough: false,
        },
        Sample {
            class: "cargo test (fail)",
            command: "cargo test",
            raw: include_str!("corpus/cargo-test-fail.txt"),
            exit_ok: false,
            min_reduction: None,
            must_contain: &[
                "test tests::broken_math ... FAILED",
                "test tests::broken_greeting ... FAILED",
                "panicked at src/lib.rs:25:9",
                "panicked at src/lib.rs:30:9",
                "assertion `left == right` failed: two plus two should make five",
                "tests::broken_greeting",
                "tests::broken_math",
                "test result: FAILED. 2 passed; 2 failed",
                "error: test failed, to rerun pass `--lib`",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "cargo build (compile error)",
            command: "cargo build",
            raw: include_str!("corpus/cargo-build-error.txt"),
            exit_ok: false,
            min_reduction: None,
            must_contain: &[
                "error[E0425]: cannot find value `missing_var` in this scope",
                "--> src/lib.rs:6:31",
                "error: could not compile `failcrate` (lib) due to 1 previous error",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "git status",
            command: "git status",
            raw: include_str!("corpus/git-status.txt"),
            exit_ok: true,
            min_reduction: None,
            must_contain: &[
                "On branch feat/bash-output-filtering",
                "modified:   src/tools/bash/mod.rs",
                "src/tools/bash/filter/",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "git diff",
            command: "git diff HEAD~2 HEAD~1",
            raw: include_str!("corpus/git-diff.txt"),
            exit_ok: true,
            min_reduction: None,
            must_contain: &[],
            // Diff hunks are the signal; declarative stripping cannot reduce
            // them safely. Structured summarizing is PR 2 of #336.
            expect_passthrough: true,
        },
        Sample {
            class: "git log",
            command: "git log -n 12",
            raw: include_str!("corpus/git-log.txt"),
            exit_ok: true,
            min_reduction: None,
            must_contain: &[],
            // Structured summarizing is PR 2 of #336.
            expect_passthrough: true,
        },
        Sample {
            class: "npm test (pass)",
            command: "npm test -- --verbose",
            raw: include_str!("corpus/npm-test-pass.txt"),
            exit_ok: true,
            min_reduction: Some(60),
            must_contain: &["Tests:       5 passed, 5 total"],
            expect_passthrough: false,
        },
        Sample {
            class: "npm test (fail)",
            command: "npm test",
            raw: include_str!("corpus/npm-test-fail.txt"),
            exit_ok: false,
            min_reduction: None,
            must_contain: &[
                "FAIL src/api.test.js",
                "● fetches user",
                "● handles missing user",
                "Expected: 404",
                "Received: 200",
                "at Object.toBe (src/api.test.js:2:47)",
                "at Object.toBe (src/api.test.js:3:55)",
                "Tests:       2 failed, 3 passed, 5 total",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "npm install (installer log)",
            command: "npm install gulp@4 request@2 bower@1",
            raw: include_str!("corpus/npm-install.txt"),
            exit_ok: true,
            min_reduction: Some(60),
            must_contain: &[
                "added 386 packages, and audited 387 packages in 22s",
                "16 vulnerabilities (10 moderate, 4 high, 2 critical)",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "shellcheck (linter)",
            command: "shellcheck /tmp/fixgen/sh/deploy.sh",
            raw: include_str!("corpus/shellcheck.txt"),
            exit_ok: false,
            min_reduction: None,
            must_contain: &["In /tmp/fixgen/sh/deploy.sh line 5:", "SC2045", "SC2086"],
            expect_passthrough: false,
        },
    ]
}

/// Rough token estimate: 4 bytes per token, the standard heuristic for
/// English/code text. Only ratios matter here, not absolute counts.
fn est_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

/// Filtered text for a sample plus whether a filter actually applied.
fn filtered(sample: &Sample) -> (String, bool) {
    match filter_output(sample.command, sample.raw, sample.exit_ok) {
        Some(f) => (f.text, true),
        None => (sample.raw.to_string(), false),
    }
}

fn reduction_pct(raw: &str, out: &str) -> f64 {
    let before = est_tokens(raw) as f64;
    let after = est_tokens(out) as f64;
    if before == 0.0 {
        return 0.0;
    }
    100.0 * (1.0 - after / before)
}

#[test]
fn corpus_noisy_classes_hit_reduction_bar() {
    for sample in samples() {
        let Some(min) = sample.min_reduction else {
            continue;
        };
        let (out, applied) = filtered(&sample);
        assert!(applied, "[{}] expected a filter to apply", sample.class);
        let pct = reduction_pct(sample.raw, &out);
        assert!(
            pct >= f64::from(min),
            "[{}] token reduction {pct:.1}% is below the {min}% bar\n--- filtered ---\n{out}",
            sample.class,
        );
    }
}

#[test]
fn corpus_failure_and_summary_content_survives_verbatim() {
    for sample in samples() {
        let (out, _) = filtered(&sample);
        for needle in sample.must_contain {
            assert!(
                out.contains(needle),
                "[{}] filtered output lost {needle:?}\n--- filtered ---\n{out}",
                sample.class,
            );
        }
    }
}

#[test]
fn corpus_uncovered_classes_pass_through_untouched() {
    for sample in samples() {
        if !sample.expect_passthrough {
            continue;
        }
        assert!(
            filter_output(sample.command, sample.raw, sample.exit_ok).is_none(),
            "[{}] expected passthrough (no PR-1 filter for this class)",
            sample.class,
        );
    }
}

#[test]
fn corpus_filter_overhead_under_10ms_per_call() {
    // Warm the embedded registry (one-time compile cost is not per-call
    // overhead).
    let _ = filter_output("cargo test", "warmup", true);
    for sample in samples() {
        // Best of three: absorbs scheduler noise in debug CI runs while still
        // failing on a real regression (the bar is per-call cost).
        let best = (0..3)
            .map(|_| {
                let start = std::time::Instant::now();
                let _ = filter_output(sample.command, sample.raw, sample.exit_ok);
                start.elapsed()
            })
            .min()
            .expect("three timed runs");
        assert!(
            best < std::time::Duration::from_millis(10),
            "[{}] filter overhead {best:?} exceeds the 10 ms bar",
            sample.class,
        );
    }
}

#[test]
fn corpus_benchmark_report() {
    // Prints the table committed to
    // docs/benchmarks/adr-0037-bash-filter-tokens.md (run with --nocapture).
    println!("| class | tokens before | tokens after | reduction | filter |");
    println!("|---|---|---|---|---|");
    for sample in samples() {
        let result = filter_output(sample.command, sample.raw, sample.exit_ok);
        let (out, name) = match &result {
            Some(f) => (f.text.as_str(), f.name.as_str()),
            None => (sample.raw, "(passthrough)"),
        };
        println!(
            "| {} | {} | {} | {:.0}% | {} |",
            sample.class,
            est_tokens(sample.raw),
            est_tokens(out),
            reduction_pct(sample.raw, out),
            name,
        );
    }
}
