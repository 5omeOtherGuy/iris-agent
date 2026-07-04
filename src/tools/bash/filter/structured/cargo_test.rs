//! Structured filter for `cargo test` (libtest output).
//!
//! Pass: one line per test binary (label + counts + duration) and a total
//! line. Fail: failure sections, `FAILED` lines, panic messages, and the
//! result summaries stay verbatim; only build/run chatter and passing-test
//! listings are dropped. No `test result:` lines and no diagnostics means
//! the output is not libtest format: decline (raw passthrough).

use std::sync::OnceLock;

use regex::Regex;

use super::super::engine::strip_ansi;

struct SuiteResult {
    label: String,
    ok: bool,
    passed: u64,
    failed: u64,
    ignored: u64,
    measured: u64,
    filtered_out: u64,
    duration_secs: Option<f64>,
}

impl SuiteResult {
    fn total_run(&self) -> u64 {
        self.passed + self.failed + self.ignored + self.measured
    }
}

fn result_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out(?:; finished in ([0-9.]+)s)?$",
        )
        .expect("static regex")
    })
}

fn running_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s+Running (.+) \(([^)]+)\)$").expect("static regex"))
}

fn doctest_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s+Doc-tests (\S+)$").expect("static regex"))
}

/// Build/run chatter dropped on both paths (PR-1 parity).
fn is_chatter(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"^\s*(Compiling|Downloading|Downloaded|Updating|Locking|Adding|Removing|Checking",
            r"|Documenting|Fresh|Blocking|Building|Finished)\s",
        ))
        .expect("static regex")
    })
    .is_match(line)
}

/// Crate name from a `target/.../deps/name-hash` binary path.
fn crate_name(path: &str) -> Option<&str> {
    let file = path.rsplit('/').next()?;
    let (name, _hash) = file.rsplit_once('-')?;
    (!name.is_empty()).then_some(name)
}

pub(super) fn apply(output: &str, exit_ok: bool) -> Option<String> {
    let text = strip_ansi(output);
    let lines: Vec<&str> = text.lines().collect();

    let mut suites: Vec<SuiteResult> = Vec::new();
    let mut pending_label: Option<String> = None;
    for line in &lines {
        if let Some(c) = running_re().captures(line) {
            let what = c.get(1).expect("group").as_str();
            pending_label = Some(match crate_name(c.get(2).expect("group").as_str()) {
                Some(name) => format!("{what} ({name})"),
                None => what.to_string(),
            });
        } else if let Some(c) = doctest_re().captures(line) {
            pending_label = Some(format!("Doc-tests {}", &c[1]));
        } else if let Some(c) = result_re().captures(line) {
            suites.push(SuiteResult {
                label: pending_label.take().unwrap_or_else(|| "tests".to_string()),
                ok: &c[1] == "ok",
                passed: c[2].parse().ok()?,
                failed: c[3].parse().ok()?,
                ignored: c[4].parse().ok()?,
                measured: c[5].parse().ok()?,
                filtered_out: c[6].parse().ok()?,
                duration_secs: c.get(7).and_then(|d| d.as_str().parse().ok()),
            });
        }
    }

    if suites.is_empty() {
        // Not libtest output. A test build that failed to compile carries
        // cargo diagnostics: reuse the cargo-build reduction. Anything else
        // is unparsable: decline.
        if lines
            .iter()
            .any(|l| l.starts_with("error") || l.starts_with("warning:"))
        {
            return super::cargo_build::apply(output, false);
        }
        return None;
    }

    let is_noise = |l: &str| {
        l.trim().is_empty()
            || is_chatter(l)
            || running_re().is_match(l)
            || doctest_re().is_match(l)
            || regex_running_n_tests(l)
            || (l.starts_with("test ") && l.ends_with("... ok"))
    };

    let all_ok = suites.iter().all(|s| s.ok && s.failed == 0);
    let any_failed_line = lines.iter().any(|l| l.ends_with("... FAILED"));
    if exit_ok && all_ok && !any_failed_line {
        // Diagnostics printed before/around the test run (compiler warnings,
        // notes, test-binary stderr) stay verbatim ahead of the summary; the
        // per-crate warning recap is redundant chatter.
        let residual: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|l| {
                !is_noise(l) && !result_re().is_match(l) && !super::cargo_build::is_warning_recap(l)
            })
            .collect();
        let summary = summarize_pass(&suites);
        if residual.is_empty() {
            return Some(summary);
        }
        return Some(format!("{}\n{summary}", residual.join("\n")));
    }

    // Failure path: keep everything verbatim except known noise.
    let kept: Vec<&str> = lines.iter().copied().filter(|l| !is_noise(l)).collect();
    if kept.is_empty() {
        return None;
    }
    Some(kept.join("\n"))
}

fn regex_running_n_tests(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^running \d+ tests?$").expect("static regex"))
        .is_match(line)
}

fn summarize_pass(suites: &[SuiteResult]) -> String {
    let mut out = Vec::new();
    for s in suites {
        if s.total_run() == 0 {
            continue; // empty suites are counted in the total line only
        }
        let mut line = format!("{}: ok. {} passed", s.label, s.passed);
        if s.ignored > 0 {
            line.push_str(&format!("; {} ignored", s.ignored));
        }
        if s.measured > 0 {
            line.push_str(&format!("; {} measured", s.measured));
        }
        if s.filtered_out > 0 {
            line.push_str(&format!("; {} filtered out", s.filtered_out));
        }
        if let Some(d) = s.duration_secs {
            line.push_str(&format!(" in {d:.2}s"));
        }
        out.push(line);
    }
    let total_passed: u64 = suites.iter().map(|s| s.passed).sum();
    let mut total = format!(
        "cargo test: ok. {} passed ({} suite{}",
        total_passed,
        suites.len(),
        if suites.len() == 1 { "" } else { "s" },
    );
    if suites.iter().all(|s| s.duration_secs.is_some()) {
        let secs: f64 = suites.iter().filter_map(|s| s.duration_secs).sum();
        total.push_str(&format!(" in {secs:.2}s"));
    }
    total.push(')');
    out.push(total);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASS: &str = "\
   Compiling alpha v0.1.0 (/w/alpha)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.84s
     Running unittests src/lib.rs (target/debug/deps/alpha-98ef8bd8b3160952)

running 2 tests
test tests::adds ... ok
test tests::adds_neg ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

   Doc-tests alpha

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
";

    #[test]
    fn pass_run_summarizes_per_binary() {
        let out = apply(PASS, true).expect("parses");
        assert_eq!(
            out,
            "unittests src/lib.rs (alpha): ok. 2 passed in 0.01s\n\
             cargo test: ok. 2 passed (2 suites in 0.03s)"
        );
    }

    #[test]
    fn pass_run_keeps_compiler_warnings_verbatim() {
        let raw = "\
   Compiling alpha v0.1.0 (/w/alpha)
warning: unused variable: `x`
 --> src/lib.rs:2:9
  |
2 |     let x = 1;
  |         ^ help: if this is intentional, prefix it with an underscore: `_x`
warning: `alpha` (lib) generated 1 warning
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.84s
     Running unittests src/lib.rs (target/debug/deps/alpha-98ef8bd8b3160952)

running 1 test
test tests::adds ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let out = apply(raw, true).expect("parses");
        assert!(out.contains("warning: unused variable: `x`"), "{out}");
        assert!(out.contains("--> src/lib.rs:2:9"), "{out}");
        assert!(!out.contains("generated 1 warning"), "{out}");
        assert!(
            out.contains("unittests src/lib.rs (alpha): ok. 1 passed in 0.01s"),
            "{out}"
        );
    }

    #[test]
    fn pass_summary_requires_exit_ok() {
        // Exit failure with all-ok summaries (e.g. a post-test harness crash)
        // must not produce a success summary; the fail path keeps the
        // unexplained lines.
        let raw = format!("{PASS}\nSegmentation fault (core dumped)\n");
        let out = apply(&raw, false).expect("fail path output");
        assert!(out.contains("Segmentation fault"), "{out}");
        assert!(!out.contains("cargo test: ok."), "{out}");
    }

    #[test]
    fn ignored_and_filtered_counts_surface() {
        let raw = "\
     Running unittests src/lib.rs (target/debug/deps/x-abc123)
test result: ok. 3 passed; 0 failed; 2 ignored; 0 measured; 7 filtered out; finished in 0.10s
";
        let out = apply(raw, true).expect("parses");
        assert!(
            out.contains("ok. 3 passed; 2 ignored; 7 filtered out in 0.10s"),
            "{out}"
        );
    }

    #[test]
    fn fail_run_keeps_failure_detail_verbatim() {
        let raw = "\
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.31s
     Running unittests src/lib.rs (target/debug/deps/foo-9c1b2a)

running 2 tests
test tests::works ... ok
test tests::broken ... FAILED

failures:

---- tests::broken stdout ----
thread 'tests::broken' panicked at src/lib.rs:42:9:
assertion `left == right` failed
  left: 1
 right: 2

failures:
    tests::broken

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

error: test failed, to rerun pass `--lib`
";
        let out = apply(raw, false).expect("fail path output");
        for needle in [
            "test tests::broken ... FAILED",
            "---- tests::broken stdout ----",
            "thread 'tests::broken' panicked at src/lib.rs:42:9:",
            "assertion `left == right` failed",
            "test result: FAILED. 1 passed; 1 failed",
            "error: test failed, to rerun pass `--lib`",
        ] {
            assert!(out.contains(needle), "lost {needle:?} in:\n{out}");
        }
        assert!(!out.contains("test tests::works ... ok"), "{out}");
        assert!(!out.contains("Finished"), "{out}");
    }

    #[test]
    fn compile_error_falls_back_to_build_reduction() {
        let raw = "\
   Compiling foo v0.1.0 (/w/foo)
error[E0308]: mismatched types
 --> src/lib.rs:5:20
error: could not compile `foo` (lib) due to 1 previous error
";
        let out = apply(raw, false).expect("diagnostics kept");
        assert!(out.contains("error[E0308]: mismatched types"), "{out}");
        assert!(out.contains("--> src/lib.rs:5:20"), "{out}");
        assert!(!out.contains("Compiling"), "{out}");
    }

    #[test]
    fn unparsable_output_declines() {
        assert_eq!(
            apply("complete garbage\nnothing test-like here", true),
            None
        );
        assert_eq!(apply("complete garbage", false), None);
    }
}
