//! Structured Rust filters for the top bash command classes (ADR-0037, PR 2
//! of #336): cargo test, cargo build/check/clippy, git status/log/diff, and
//! npm/pnpm test (jest/vitest).
//!
//! Unlike the declarative TOML pipelines, these parse the output before
//! summarizing, so they can produce per-binary test summaries, per-file diff
//! stats, and compact commit lines that line regexes cannot. Parsing
//! approaches and summary shapes are informed by RTK's structured filters
//! (rtk-ai/rtk, Apache-2.0 -- see `data/NOTICE.md`); the code is original to
//! Iris and parses post-hoc (RTK rewrites the invocation instead).
//!
//! Contract per filter (`apply: fn(&str, bool) -> Option<String>`):
//! - `None` means "cannot parse this confidently" and yields the raw output
//!   at the seam -- a structured filter never guesses;
//! - success summaries are only produced when `exit_ok` is true;
//! - failure detail (failing test names, panic messages, `file:line`
//!   references, compiler diagnostics, diff hunks) is kept verbatim;
//! - only known-noise lines are ever dropped on failure paths.
//!
//! Dispatch runs ahead of the TOML registry in [`super::filter_output`]; a
//! matched structured filter never falls through to a TOML filter (declining
//! means raw passthrough, not a second reduction attempt).

mod cargo_build;
mod cargo_test;
mod git_diff;
mod git_log;
mod git_status;
mod npm_test;

/// A structured filter selected for the effective command.
pub(super) struct StructuredFilter {
    /// Name for the provenance notice (matches the retired TOML filter names
    /// where one existed).
    pub(super) name: &'static str,
    /// `(output, exit_ok) -> Option<filtered>`; `None` = decline (raw).
    pub(super) apply: fn(&str, bool) -> Option<String>,
}

/// Find the structured filter for an effective command (as produced by
/// `command::effective_command`). Matching is token-based and conservative:
/// anything ambiguous returns `None` and dispatch falls back to the TOML
/// registry.
pub(super) fn find(effective: &str) -> Option<StructuredFilter> {
    let tokens: Vec<&str> = effective.split_whitespace().collect();
    let (&program, args) = tokens.split_first()?;
    match program {
        "cargo" => {
            // Skip a `+toolchain` selector.
            let args = match args.split_first() {
                Some((t, rest)) if t.starts_with('+') => rest,
                _ => args,
            };
            match args.first().copied()? {
                "t" | "test" => Some(StructuredFilter {
                    name: "cargo-test",
                    apply: cargo_test::apply,
                }),
                // `cargo run` is deliberately uncovered: after the chatter
                // comes arbitrary program output, which must stay raw.
                "b" | "build" | "c" | "check" | "clippy" | "fix" | "doc" => {
                    Some(StructuredFilter {
                        name: "cargo-build",
                        apply: cargo_build::apply,
                    })
                }
                _ => None,
            }
        }
        "git" => match git_subcommand(args)? {
            "status" => Some(StructuredFilter {
                name: "git-status",
                apply: git_status::apply,
            }),
            "log" => Some(StructuredFilter {
                name: "git-log",
                apply: git_log::apply,
            }),
            "diff" => Some(StructuredFilter {
                name: "git-diff",
                apply: git_diff::apply,
            }),
            _ => None,
        },
        "npm" | "pnpm" | "yarn" | "bun" => {
            let is_test = match args.first().copied()? {
                "t" | "test" | "tst" => true,
                "run" => args
                    .get(1)
                    .is_some_and(|s| *s == "test" || s.starts_with("test:")),
                _ => false,
            };
            is_test.then_some(StructuredFilter {
                name: "npm-test",
                apply: npm_test::apply,
            })
        }
        "npx" => match args.first().copied()? {
            "jest" | "vitest" => Some(StructuredFilter {
                name: "npm-test",
                apply: npm_test::apply,
            }),
            _ => None,
        },
        "jest" | "vitest" => Some(StructuredFilter {
            name: "npm-test",
            apply: npm_test::apply,
        }),
        _ => None,
    }
}

/// Extract the git subcommand, skipping known global flags. Unknown leading
/// flags return `None` (conservative: never guess the subcommand).
fn git_subcommand<'a>(args: &[&'a str]) -> Option<&'a str> {
    let mut i = 0;
    while let Some(&arg) = args.get(i) {
        if !arg.starts_with('-') {
            return Some(arg);
        }
        match arg {
            // Global flags that consume a separate value token.
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path" => i += 2,
            // Known value-less or `--flag=value` global flags.
            "-P"
            | "--no-pager"
            | "--paginate"
            | "--no-optional-locks"
            | "--literal-pathspecs"
            | "--no-replace-objects"
            | "--bare" => i += 1,
            _ if arg.starts_with("--") && arg.contains('=') => i += 1,
            // Anything else: refuse to guess.
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_for(effective: &str) -> Option<&'static str> {
        find(effective).map(|f| f.name)
    }

    #[test]
    fn cargo_dispatch() {
        assert_eq!(name_for("cargo test"), Some("cargo-test"));
        assert_eq!(name_for("cargo t --workspace"), Some("cargo-test"));
        assert_eq!(name_for("cargo +nightly test"), Some("cargo-test"));
        assert_eq!(name_for("cargo build --release"), Some("cargo-build"));
        assert_eq!(name_for("cargo check"), Some("cargo-build"));
        assert_eq!(name_for("cargo clippy -- -D warnings"), Some("cargo-build"));
        // `cargo run` relays arbitrary program output; deliberately unmatched.
        assert_eq!(name_for("cargo run"), None);
        assert_eq!(name_for("cargo r"), None);
        // nextest output is not libtest format; deliberately unmatched.
        assert_eq!(name_for("cargo nextest run"), None);
        assert_eq!(name_for("cargo fmt"), None);
        assert_eq!(name_for("cargo"), None);
    }

    #[test]
    fn git_dispatch() {
        assert_eq!(name_for("git status"), Some("git-status"));
        assert_eq!(name_for("git -C /some/path status"), Some("git-status"));
        assert_eq!(name_for("git -c color.ui=false log -n 5"), Some("git-log"));
        assert_eq!(name_for("git --no-pager diff HEAD~1"), Some("git-diff"));
        assert_eq!(name_for("git log --oneline"), Some("git-log"));
        assert_eq!(name_for("git commit -m x"), None);
        // Unknown leading flag: refuse to guess the subcommand.
        assert_eq!(name_for("git --weird-flag status"), None);
        assert_eq!(name_for("git"), None);
    }

    #[test]
    fn npm_test_dispatch() {
        assert_eq!(name_for("npm test"), Some("npm-test"));
        assert_eq!(name_for("npm t"), Some("npm-test"));
        assert_eq!(name_for("npm test -- --verbose"), Some("npm-test"));
        assert_eq!(name_for("pnpm test"), Some("npm-test"));
        assert_eq!(name_for("npm run test"), Some("npm-test"));
        assert_eq!(name_for("pnpm run test:unit"), Some("npm-test"));
        assert_eq!(name_for("npx vitest run"), Some("npm-test"));
        assert_eq!(name_for("npx jest"), Some("npm-test"));
        assert_eq!(name_for("vitest run"), Some("npm-test"));
        // npm install keeps its TOML filter; not a structured match.
        assert_eq!(name_for("npm install"), None);
        assert_eq!(name_for("npm run build"), None);
    }

    #[test]
    fn unrelated_commands_do_not_match() {
        assert_eq!(name_for("ls -la"), None);
        assert_eq!(name_for("shellcheck x.sh"), None);
    }
}
