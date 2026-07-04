//! Structured filter for `cargo build`/`check`/`clippy` (and the other
//! compile-and-report subcommands: `fix`, `doc`). `cargo run` is deliberately
//! not covered: its output is arbitrary program output that must never be
//! mistaken for cargo chatter.
//!
//! Success with nothing to report reduces to `ok`. Diagnostics (errors,
//! warnings, notes, help, source excerpts) stay verbatim; only per-crate
//! status chatter and the per-crate `warning: `x` generated N warnings`
//! recap lines are dropped.

use std::sync::OnceLock;

use regex::Regex;

use super::super::engine::strip_ansi;

/// Per-crate progress/status chatter.
fn is_chatter(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"^\s*(Compiling|Checking|Downloading|Downloaded|Updating|Locking|Adding|Removing",
            r"|Documenting|Fresh|Blocking|Building|Finished|Installing|Installed)\s",
            r"|^\s+Running `",
        ))
        .expect("static regex")
    })
    .is_match(line)
}

/// Per-crate warning-count recap (`warning: `foo` (lib) generated 3 warnings
/// ...`). The warnings themselves are kept; the recap is redundant.
pub(super) fn is_warning_recap(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^warning: .+ generated \d+ warnings?( .*)?$").expect("static regex")
    })
    .is_match(line)
}

pub(super) fn apply(output: &str, exit_ok: bool) -> Option<String> {
    let text = strip_ansi(output);
    let mut recognized = false;
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| {
            if is_chatter(l) || is_warning_recap(l) {
                recognized = true;
                return false;
            }
            !l.trim().is_empty()
        })
        .collect();
    if !recognized {
        // No cargo chatter recognized: this is not cargo-shaped output.
        // Decline rather than guess (blank-stripping alone is not a filter).
        return None;
    }
    if kept.is_empty() {
        // Everything was chatter. Only a clean exit may summarize as ok;
        // a failed command with unrecognized output passes through raw.
        return exit_ok.then(|| "ok".to_string());
    }
    Some(kept.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_build_reduces_to_ok() {
        let raw = "\
   Compiling proc-macro2 v1.0.86
   Compiling serde v1.0.204
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 24.31s
";
        assert_eq!(apply(raw, true).as_deref(), Some("ok"));
    }

    #[test]
    fn clean_looking_output_with_failed_exit_declines() {
        let raw = "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.00s\n";
        assert_eq!(apply(raw, false), None);
    }

    #[test]
    fn warnings_survive_chatter_dropped() {
        let raw = "\
    Checking beta v0.1.0 (/w/beta)
warning: unused variable: `unused`
 --> crates/beta/src/lib.rs:1:41
  |
1 | pub fn add(a: i32, b: i32) -> i32 { let unused = 7; a + b }
  |                                         ^^^^^^ help: if this is intentional, prefix it with an underscore: `_unused`
  |
  = note: `#[warn(unused_variables)]` (part of `#[warn(unused)]`) on by default

warning: `beta` (lib) generated 1 warning (run `cargo fix --lib -p beta` to apply 1 suggestion)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.15s
";
        let out = apply(raw, true).expect("reduces");
        assert!(out.contains("warning: unused variable: `unused`"), "{out}");
        assert!(out.contains("--> crates/beta/src/lib.rs:1:41"), "{out}");
        assert!(!out.contains("Checking beta"), "{out}");
        assert!(!out.contains("generated 1 warning"), "{out}");
        assert!(!out.contains("Finished"), "{out}");
    }

    #[test]
    fn compile_error_kept_verbatim() {
        let raw = "\
   Compiling foo v0.1.0 (/w/foo)
error[E0425]: cannot find value `nope` in this scope
 --> src/main.rs:2:13
  |
2 |     let y = nope;
  |             ^^^^ not found in this scope
error: could not compile `foo` (bin \"foo\") due to 1 previous error
";
        let out = apply(raw, false).expect("reduces");
        assert!(
            out.contains("error[E0425]: cannot find value `nope` in this scope"),
            "{out}"
        );
        assert!(
            out.contains("error: could not compile `foo` (bin \"foo\") due to 1 previous error"),
            "{out}"
        );
        assert!(!out.contains("Compiling"), "{out}");
    }

    #[test]
    fn unparsable_output_declines() {
        assert_eq!(apply("no cargo lines here\njust text", true), None);
        assert_eq!(apply("no cargo lines here", false), None);
        // Blank lines alone are not recognition: still a decline.
        assert_eq!(apply("garbage\n\nmore garbage", true), None);
    }
}
