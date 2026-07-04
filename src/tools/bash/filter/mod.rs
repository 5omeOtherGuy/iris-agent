//! Command-output filtering for the `bash` tool (ADR-0037).
//!
//! One seam: captured output is filtered after the command exits and before
//! `truncate_tail`, for one-shot runs, persistent-session runs, and finalized
//! background jobs. Filters are declarative TOML pipelines embedded at build
//! time (`data/*.toml`, concatenated by `build.rs`); the engine lives in
//! [`engine`], dispatch keying in [`command`].
//!
//! Fail-safe contract (ADR-0036 "without quality loss"):
//! - any filter error or panic yields the raw output;
//! - a filter that empties non-empty output (without an explicit `on_empty`
//!   message) yields the raw output;
//! - exit codes and status footers are appended by the caller *after*
//!   filtering and are never touched;
//! - `raw: true` on the tool call bypasses filtering entirely;
//! - the full raw output remains reachable via session handles (ADR-0011).

mod command;
mod engine;

use std::sync::OnceLock;

/// All built-in filter definitions, concatenated from
/// `src/tools/bash/filter/data/*.toml` by `build.rs`.
const BUILTIN_TOML: &str = include_str!(concat!(env!("OUT_DIR"), "/bash_builtin_filters.toml"));

fn registry() -> &'static [engine::CompiledFilter] {
    static REGISTRY: OnceLock<Vec<engine::CompiledFilter>> = OnceLock::new();
    REGISTRY.get_or_init(|| match engine::parse_and_compile(BUILTIN_TOML, "builtin") {
        Ok(filters) => filters,
        Err(e) => {
            // Fail-safe: a broken embedded blob disables filtering entirely
            // rather than failing tool calls. A unit test keeps this path dead.
            tracing::warn!(error = %e, "builtin bash filters failed to load; filtering disabled");
            Vec::new()
        }
    })
}

/// A successfully applied filter: the reduced text plus the filter name for
/// the caller's provenance marker.
pub(super) struct Filtered {
    pub(super) text: String,
    pub(super) name: String,
}

/// Filter captured command output. Returns `None` whenever the output should
/// pass through unchanged: no filter matched, the filter was a no-op, or any
/// quality guard fired. `exit_ok` must be true only when the command exited 0.
pub(super) fn filter_output(command: &str, output: &str, exit_ok: bool) -> Option<Filtered> {
    if output.trim().is_empty() {
        return None;
    }
    let effective = command::effective_command(command)?;
    let filter = registry().iter().find(|f| f.matches(&effective))?;
    let filtered = run_guarded(
        || engine::apply_filter(filter, output, exit_ok),
        &filter.name,
    )?;
    // Empty-guard: a filter must never silently swallow non-empty output. An
    // intentional empty result goes through `on_empty` (which produces a
    // message, not an empty string).
    if filtered.trim().is_empty() {
        return None;
    }
    // No-op: avoid a provenance marker when nothing was reduced.
    if filtered == output.trim_end_matches('\n') || filtered == output {
        return None;
    }
    Some(Filtered {
        text: filtered,
        name: filter.name.clone(),
    })
}

/// Run one filter application with panic containment: a panicking filter
/// yields `None` (raw output) instead of poisoning the tool call.
fn run_guarded(apply: impl FnOnce() -> String, name: &str) -> Option<String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(apply)) {
        Ok(text) => Some(text),
        Err(_) => {
            tracing::warn!(filter = %name, "bash filter panicked; returning raw output");
            None
        }
    }
}

#[cfg(test)]
mod corpus;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_filters_load_and_are_nonempty() {
        let filters = engine::parse_and_compile(BUILTIN_TOML, "builtin")
            .expect("embedded filter blob must parse");
        assert!(!filters.is_empty(), "no builtin filters compiled");
        // Every definition in the blob must have compiled (compile drops are
        // warnings at runtime, but a vendored file that fails to compile is a
        // bug caught here).
        let parsed = engine::parse_file(BUILTIN_TOML).unwrap();
        assert_eq!(
            filters.len(),
            parsed.filter_count(),
            "some builtin filter definition failed to compile"
        );
    }

    #[test]
    fn every_builtin_filter_has_inline_tests_and_they_pass() {
        let file = engine::parse_file(BUILTIN_TOML).expect("blob parses");
        let filters = engine::parse_and_compile(BUILTIN_TOML, "builtin").unwrap();
        let mut failures = Vec::new();
        for f in &filters {
            let Some(tests) = file.tests.get(&f.name) else {
                failures.push(format!("filter '{}' has no inline tests", f.name));
                continue;
            };
            assert!(
                !tests.is_empty(),
                "filter '{}' has an empty test list",
                f.name
            );
            for t in tests {
                let actual = engine::apply_filter(f, &t.input, true);
                // TOML multiline strings carry a trailing newline; compare
                // trimmed like RTK's verify does.
                if actual.trim_end_matches('\n') != t.expected.trim_end_matches('\n') {
                    failures.push(format!(
                        "[{}] {}\n--- expected ---\n{}\n--- actual ---\n{}",
                        f.name,
                        t.name,
                        t.expected.trim_end_matches('\n'),
                        actual.trim_end_matches('\n'),
                    ));
                }
            }
        }
        // Tests must not reference unknown filters (typo guard).
        let names: std::collections::HashSet<_> = filters.iter().map(|f| f.name.as_str()).collect();
        for name in file.tests.keys() {
            assert!(
                names.contains(name.as_str()),
                "[[tests.{name}]] references an unknown filter"
            );
        }
        assert!(
            failures.is_empty(),
            "{} inline test failure(s):\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }

    #[test]
    fn filter_output_applies_matching_filter() {
        let out = filter_output(
            "shellcheck script.sh",
            "In script.sh line 3:\n\nfoo\n\n",
            true,
        )
        .expect("shellcheck filter should match and reduce");
        assert_eq!(out.name, "shellcheck");
        assert_eq!(out.text, "In script.sh line 3:\nfoo");
    }

    #[test]
    fn filter_output_passthrough_when_no_match() {
        assert!(filter_output("some-unknown-tool --flag", "a\n\nb\n", true).is_none());
    }

    #[test]
    fn filter_output_passthrough_on_empty_output() {
        assert!(filter_output("shellcheck x.sh", "   \n", true).is_none());
    }

    #[test]
    fn filter_output_dispatches_on_last_segment() {
        let out = filter_output(
            "cd /some/dir && shellcheck script.sh",
            "In script.sh line 3:\n\nfoo\n",
            true,
        );
        assert!(out.is_some(), "filter must dispatch through the cd prefix");
    }

    #[test]
    fn panicking_filter_yields_raw() {
        assert_eq!(run_guarded(|| panic!("boom"), "test"), None);
        assert_eq!(
            run_guarded(|| "ok".to_string(), "test"),
            Some("ok".to_string())
        );
    }
}
