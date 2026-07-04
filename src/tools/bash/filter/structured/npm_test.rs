//! Structured filter for npm/pnpm/yarn/bun test runs (jest and vitest text
//! output).
//!
//! Pass: the summary block alone (jest `Test Suites:`/`Tests:`/... lines,
//! vitest `Test Files`/`Tests`/`Duration`). Fail: failure blocks, code
//! frames, and summaries stay verbatim; only per-test pass ticks, `PASS`
//! suite lines, runner banners, and blanks are dropped. Output from other
//! runners (no recognizable summary) declines to raw.

use std::sync::OnceLock;

use regex::Regex;

use super::super::engine::strip_ansi;

fn is_jest_summary(line: &str) -> bool {
    line.starts_with("Test Suites:")
        || line.starts_with("Tests:")
        || line.starts_with("Snapshots:")
        || line.starts_with("Time:")
}

fn vitest_summary_key(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    ["Test Files", "Tests", "Duration", "Errors"]
        .into_iter()
        .find(|key| t.starts_with(key) && t.len() > key.len() && t.as_bytes()[key.len()] == b' ')
}

fn has_failure_signal(text: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\bFAIL\b|✕|✗|×|[1-9]\d* failed|\bERR\b|Error:").expect("static regex")
    })
    .is_match(text)
}

/// Noise dropped on the failure path (PR-1 parity plus vitest banners).
fn is_noise(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"^\s*$|^> |^\s*(✓|✔|√) |^\s*PASS\b|^\s*(RUN|RUNS)\b",
            r"|^\s*Ran all test suites|^\s*Start at\s",
        ))
        .expect("static regex")
    })
    .is_match(line)
}

pub(super) fn apply(output: &str, exit_ok: bool) -> Option<String> {
    let text = strip_ansi(output);
    let lines: Vec<&str> = text.lines().collect();
    let jest = lines.iter().any(|l| l.starts_with("Test Suites:"));
    let vitest = !jest
        && lines
            .iter()
            .any(|l| vitest_summary_key(l) == Some("Test Files"));
    if !jest && !vitest {
        return None; // unrecognized runner output
    }

    if exit_ok && !has_failure_signal(&text) {
        // Pass: summary block only.
        let mut kept: Vec<String> = Vec::new();
        for line in &lines {
            if jest && is_jest_summary(line) {
                kept.push((*line).to_string());
            } else if vitest {
                match vitest_summary_key(line) {
                    Some("Duration") => {
                        // Drop the per-phase breakdown in parentheses.
                        let t = line.trim();
                        kept.push(t.split(" (").next().unwrap_or(t).to_string());
                    }
                    Some(_) => kept.push(line.trim().to_string()),
                    None => {}
                }
            }
        }
        if kept.is_empty() {
            return None;
        }
        return Some(kept.join("\n"));
    }

    // Failure (or unexpected exit): keep everything except known noise.
    let kept: Vec<&str> = lines.iter().copied().filter(|l| !is_noise(l)).collect();
    if kept.is_empty() {
        return None;
    }
    Some(kept.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const JEST_PASS: &str = "\
> myapp@1.0.0 test
> jest --verbose

PASS src/utils.test.js
  ✓ parses config (3 ms)
  ✓ merges defaults (1 ms)
PASS src/api.test.js
  ✓ fetches user (12 ms)

Test Suites: 2 passed, 2 total
Tests:       3 passed, 3 total
Snapshots:   0 total
Time:        1.802 s
Ran all test suites.
";

    #[test]
    fn jest_pass_reduces_to_summary_block() {
        let out = apply(JEST_PASS, true).expect("parses");
        assert_eq!(
            out,
            "Test Suites: 2 passed, 2 total\n\
             Tests:       3 passed, 3 total\n\
             Snapshots:   0 total\n\
             Time:        1.802 s"
        );
    }

    #[test]
    fn jest_fail_keeps_failure_blocks_verbatim() {
        let raw = "\
> myapp@1.0.0 test
> jest

PASS src/utils.test.js
  ✓ parses config (3 ms)
FAIL src/api.test.js
  ✕ fetches user (14 ms)

  ● fetches user

    expect(received).toEqual(expected) // deep equality

    Expected: 200
    Received: 404

      at Object.<anonymous> (src/api.test.js:13:22)

Test Suites: 1 failed, 1 passed, 2 total
Tests:       1 failed, 1 passed, 2 total
Time:        2.113 s
";
        let out = apply(raw, false).expect("fail path output");
        for needle in [
            "FAIL src/api.test.js",
            "● fetches user",
            "Expected: 200",
            "Received: 404",
            "at Object.<anonymous> (src/api.test.js:13:22)",
            "Tests:       1 failed, 1 passed, 2 total",
        ] {
            assert!(out.contains(needle), "lost {needle:?} in:\n{out}");
        }
        assert!(!out.contains("PASS src/utils.test.js"), "{out}");
        assert!(!out.contains("✓ parses config"), "{out}");
    }

    const VITEST_PASS: &str = "\n RUN  v4.1.9 /tmp/proj\n\n\n Test Files  1 passed (1)\n      Tests  3 passed (3)\n   Start at  18:08:42\n   Duration  256ms (transform 33ms, setup 0ms, import 59ms, tests 5ms, environment 0ms)\n";

    #[test]
    fn vitest_pass_reduces_to_summary() {
        let out = apply(VITEST_PASS, true).expect("parses");
        assert_eq!(
            out,
            "Test Files  1 passed (1)\nTests  3 passed (3)\nDuration  256ms"
        );
    }

    #[test]
    fn vitest_fail_keeps_failure_blocks() {
        let raw = "\n RUN  v4.1.9 /tmp/proj

 ❯ sum.test.mjs (3 tests | 2 failed) 34ms
     × adds negatives 19ms

⎯⎯⎯⎯⎯⎯⎯ Failed Tests 2 ⎯⎯⎯⎯⎯⎯⎯

 FAIL  sum.test.mjs > sum > adds negatives
AssertionError: expected -5 to be -6 // Object.is equality

 ❯ sum.test.mjs:6:52

 Test Files  1 failed (1)
      Tests  2 failed | 1 passed (3)
   Start at  18:08:54
   Duration  602ms (transform 87ms, setup 0ms, import 130ms, tests 34ms, environment 0ms)
";
        let out = apply(raw, false).expect("fail path output");
        for needle in [
            "FAIL  sum.test.mjs > sum > adds negatives",
            "AssertionError: expected -5 to be -6",
            "sum.test.mjs:6:52",
            "Tests  2 failed | 1 passed (3)",
        ] {
            assert!(out.contains(needle), "lost {needle:?} in:\n{out}");
        }
        assert!(!out.contains("RUN  v4.1.9"), "{out}");
        assert!(!out.contains("Start at"), "{out}");
    }

    #[test]
    fn pass_summary_requires_exit_ok() {
        let out = apply(JEST_PASS, false).expect("fail path");
        // Not the compact summary: the fail path keeps the suite lines'
        // context minus noise -- here everything except noise is summary,
        // so the summary must still be present.
        assert!(out.contains("Tests:       3 passed, 3 total"), "{out}");
    }

    #[test]
    fn unrecognized_runner_output_declines() {
        assert_eq!(apply("  5 passing (12ms)\n  1 failing\n", false), None);
        assert_eq!(apply("random text", true), None);
    }
}
