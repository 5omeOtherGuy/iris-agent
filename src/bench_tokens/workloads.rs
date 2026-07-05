//! Workload definitions: the fixture, the prompt, the scripted replay
//! sequence, the mechanical (harness-side, no-shell) success check, and the
//! verbatim needles the tool output must surface in both arms.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::nexus::{AssistantTurn, CompletionReason, ToolCall};

use super::fixtures::{build_chained_provider_tree, build_find_tree, build_repair_noise_tree};

/// The result of a workload's mechanical success check.
pub(crate) struct Outcome {
    pub(crate) success: bool,
    pub(crate) detail: String,
}

/// How a workload's tool calls are approved.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalProfile {
    /// Auto preset + a denying zero-prompt gate: only auto-safe tools
    /// (read/grep/find/ls + clean edit) can run; a prompt means the run is
    /// invalid. The no-bash workloads.
    DenyGateNoPrompts,
    /// `--dangerously-skip-permissions` (ADR-0049): the gate is bypassed for
    /// every gated call, so `bash` can run (build/test loops). The denying
    /// gate is still installed and asserted UN-consulted, and the dangerous
    /// auto-approval is asserted to have fired. Confined to a temp workspace.
    SkipPermissions,
}

pub(crate) struct Workload {
    pub(crate) name: &'static str,
    pub(crate) fixture: &'static str,
    pub(crate) prompt: &'static str,
    /// The scripted tool-call sequence the replay provider replays (real path
    /// ignores it; the real model chooses its own calls).
    pub(crate) script: fn() -> Vec<AssistantTurn>,
    /// Mechanical success check run OUTSIDE the agent turn (harness-side) for
    /// no-bash workloads; a bash workload may run its check via the agent's own
    /// shell output instead.
    pub(crate) check: fn(&Path, &str) -> Outcome,
    /// How this workload's tool calls are approved (deny-gate vs skip-perms).
    pub(crate) approval: ApprovalProfile,
    /// Facts the tool outputs MUST surface verbatim for the task to be solvable
    /// from context. Asserted present in the transcript the agent saw in BOTH
    /// arms, so a reduction that dropped an actionable fact fails the run even
    /// though the scripted answer would still mention it.
    pub(crate) needles: &'static [&'static str],
    /// Optional programmatic workspace builder, run after the fixture is
    /// materialized -- for inputs too large to commit (e.g. the >1000-file tree
    /// the find probe needs). `None` for ordinary committed-fixture workloads.
    pub(crate) build: Option<fn(&Path)>,
    /// For repair-loop bash workloads, require the model to actually reproduce
    /// a failing test before the final passing test. This prevents a shortcut
    /// where the model reads enough code to patch first and only runs a green
    /// test, which would no longer exercise the intended chained workflow.
    pub(crate) require_failing_then_passing_bash: bool,
}

pub(crate) fn workloads() -> Vec<Workload> {
    vec![
        Workload {
            name: "fix-failing-test",
            fixture: "workload1_fix_test",
            prompt: "The test `parse_len_counts_all_tokens` fails. Find and fix the bug in \
                     parse_len using read/grep/find and edit only. Do not run any shell \
                     commands; the test will be run for you.",
            script: script_fix_test,
            check: check_fix_test,
            approval: ApprovalProfile::DenyGateNoPrompts,
            // The grep across files must surface the buggy symbol and the read
            // must surface the buggy expression the fix targets.
            needles: &["parse_len", "split_whitespace().count() - 1"],
            build: None,
            require_failing_then_passing_bash: false,
        },
        Workload {
            name: "multi-file-search-and-edit",
            fixture: "workload2_rename",
            prompt: "Rename the identifier MAX_RETRIES to MAX_ATTEMPTS everywhere it appears \
                     in this tree (code and docs). Use grep/find to locate every occurrence \
                     and edit to change them. Do not run any shell commands.",
            script: script_rename,
            check: check_rename,
            approval: ApprovalProfile::DenyGateNoPrompts,
            // The grep must surface the identifier being renamed.
            needles: &["MAX_RETRIES"],
            build: None,
            require_failing_then_passing_bash: false,
        },
        Workload {
            name: "investigate-large-log",
            fixture: "workload3_log_triage",
            prompt: "One test failed with a token-budget ceiling assertion. Search the logs/ \
                     directory to find which test failed and the exact left/right values it \
                     reported. Answer in one sentence. Do not run any shell commands.",
            script: script_log_triage,
            check: check_log_triage,
            approval: ApprovalProfile::DenyGateNoPrompts,
            // The reduced grep/read output must still carry the planted fact
            // (test name + both drift values), or the task is not solvable from
            // context in arm A.
            needles: &["ceiling_is_exact", "8192", "8191"],
            build: None,
            require_failing_then_passing_bash: false,
        },
    ]
}

/// Headline workloads, optionally narrowed by `IRIS_BENCH_WORKLOAD` (a
/// comma-separated list of workload names). Unset or empty -> all three. Lets a
/// focused real run target one cell (e.g. the cheap `investigate-large-log`)
/// without paying for the others. An entirely non-matching filter yields an
/// empty set, which the caller surfaces via the printed workload count.
pub(crate) fn selected_workloads() -> Vec<Workload> {
    filter_by_env(workloads(), "IRIS_BENCH_WORKLOAD")
}

/// Bash-enabled workloads filtered by the same `IRIS_BENCH_WORKLOAD` knob as
/// the headline matrix. Kept separate so callers still have to opt into the
/// dangerous bash approval path explicitly.
pub(crate) fn selected_bash_workloads() -> Vec<Workload> {
    filter_by_env(bash_workloads(), "IRIS_BENCH_WORKLOAD")
}

fn filter_by_env(all: Vec<Workload>, env: &str) -> Vec<Workload> {
    let filter = std::env::var(env).unwrap_or_default();
    let names: Vec<&str> = filter
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        return all;
    }
    all.into_iter()
        .filter(|w| names.contains(&w.name))
        .collect()
}

/// Bash-enabled workloads (require `--dangerously-skip-permissions`). Kept
/// separate from `workloads()` so the deny-gate replay + no-bash headline paths
/// never touch a bash workload. Phase 4: one read-only diagnosis task -- run
/// the failing tests and report the failure facts; no file is mutated.
pub(crate) fn bash_workloads() -> Vec<Workload> {
    vec![
        Workload {
            name: "bash-diagnose-test-failure",
            fixture: "workload4_bash_diagnose",
            prompt: "The crate's tests fail. Run the tests, read the failure, and report which \
                     test failed and the exact left/right values it asserted. Answer in one \
                     sentence. Do not edit any files.",
            script: script_bash_diagnose,
            check: check_bash_diagnose,
            approval: ApprovalProfile::SkipPermissions,
            // The reduced bash output must still carry the planted assertion values.
            needles: &["8191", "8192"],
            build: None,
            require_failing_then_passing_bash: false,
        },
        Workload {
            name: "chained-openai-summary-fix",
            fixture: "workload5_chained_provider_fix",
            prompt: "Live reasoning summaries do not stream for the OpenAI Codex Responses \
                     provider even when reasoning effort is enabled. This fixture mirrors a \
                     recent Iris bug. First, before reading or editing any file, run \
                     `cargo test` to reproduce the failure. Then discover the relevant files \
                     with find/grep/read, fix the request builder without weakening tests, \
                     and run `cargo test` until it passes. Summarize the fix.",
            script: script_chained_openai_summary_fix,
            check: check_chained_openai_summary_fix,
            approval: ApprovalProfile::SkipPermissions,
            needles: &[
                "reasoning_request_asks_for_summary_so_live_thinking_can_stream",
                "summary: None",
                "summary:auto",
            ],
            build: Some(build_chained_provider_tree),
            require_failing_then_passing_bash: true,
        },
        Workload {
            name: "chained-iris-recall-span-fix",
            fixture: "workload6_iris_recall_span",
            prompt: "Recall by standalone span fails and out-of-range spans are not rejected. \
                     This fixture mirrors Iris PR #393. First, before reading or editing any \
                     file, run `cargo test` to reproduce the failure. Then use find/grep/read \
                     to locate the recall implementation, fix it without weakening tests, and \
                     run `cargo test` until it passes. Summarize the fix.",
            script: script_chained_iris_recall_span_fix,
            check: check_chained_iris_recall_span_fix,
            approval: ApprovalProfile::SkipPermissions,
            needles: &[
                "standalone_span_without_handle_returns_original_turns",
                "missing recall handle",
                "selected no turns",
            ],
            build: Some(build_repair_noise_tree),
            require_failing_then_passing_bash: true,
        },
        Workload {
            name: "chained-iris-fold-resume-fix",
            fixture: "workload7_iris_fold_resume",
            prompt: "Resume after a persisted fold corrupts the durable id chain. This \
                     fixture mirrors Iris PR #394. First, before reading or editing any file, \
                     run `cargo test` to reproduce the failure. Then use find/grep/read to \
                     locate the resume scanner, fix it without weakening tests, and run \
                     `cargo test` until it passes. Summarize the fix.",
            script: script_chained_iris_fold_resume_fix,
            check: check_chained_iris_fold_resume_fix,
            approval: ApprovalProfile::SkipPermissions,
            needles: &[
                "resume_after_a_fold_keeps_the_fold_in_the_durable_id_chain",
                "modelSelection",
                "fold",
            ],
            build: Some(build_repair_noise_tree),
            require_failing_then_passing_bash: true,
        },
        Workload {
            name: "chained-ampi-github-token-fix",
            fixture: "workload8_ampi_github_auth",
            prompt: "GitHub auth does not pick up common token environment variables. This \
                     fixture mirrors ampi PR #224. First, before reading or editing any file, \
                     run `npm test` to reproduce the failure. Then use find/grep/read to locate \
                     the auth resolver, fix it without weakening tests, and run `npm test` \
                     until it passes. Summarize the fix.",
            script: script_chained_ampi_github_token_fix,
            check: check_chained_ampi_github_token_fix,
            approval: ApprovalProfile::SkipPermissions,
            needles: &[
                "GH_TOKEN",
                "GITHUB_PERSONAL_ACCESS_TOKEN",
                "AMPI_GITHUB_TOKEN",
            ],
            build: Some(build_repair_noise_tree),
            require_failing_then_passing_bash: true,
        },
        Workload {
            name: "chained-ampi-pack-untracked-fix",
            fixture: "workload9_ampi_pack_untracked",
            prompt: "Publish verification allows untracked files in the npm dry-run tarball. \
                     This fixture mirrors ampi PR #223. First, before reading or editing any \
                     file, run `npm test` to reproduce the failure. Then use find/grep/read to \
                     locate the pack verification code, fix it without weakening tests, and \
                     run `npm test` until it passes. Summarize the fix.",
            script: script_chained_ampi_pack_untracked_fix,
            check: check_chained_ampi_pack_untracked_fix,
            approval: ApprovalProfile::SkipPermissions,
            needles: &[
                "untracked files in package",
                "debug.log",
                "private files in package",
            ],
            build: Some(build_repair_noise_tree),
            require_failing_then_passing_bash: true,
        },
        Workload {
            name: "chained-ampi-private-docs-fix",
            fixture: "workload10_ampi_private_docs",
            prompt: "The npm package file list includes docs/private. This fixture mirrors \
                     ampi PR #222. First, before reading or editing any file, run `npm test` \
                     to reproduce the failure. Then use find/grep/read to locate the file-list \
                     filter, fix it without weakening tests, and run `npm test` until it \
                     passes. Summarize the fix.",
            script: script_chained_ampi_private_docs_fix,
            check: check_chained_ampi_private_docs_fix,
            approval: ApprovalProfile::SkipPermissions,
            needles: &[
                "docs/private/operator-notes.md",
                "publishedFiles",
                "startsWith('.')",
            ],
            build: Some(build_repair_noise_tree),
            require_failing_then_passing_bash: true,
        },
    ]
}

/// Per-tool live model-probe workloads (Phase 5, layer 2). Each asks a real
/// model an EXACT question whose answer lives in one tool's (reduced) output,
/// so success is scored mechanically (the exact value in the answer) and the
/// arm A vs B comparison shows whether the reduced output still lets the model
/// answer -- paired with the deterministic render probe in `probes.rs`. Uses
/// the deny gate (grep/find/ls/read are auto-safe; no bash).
pub(crate) fn probe_workloads() -> Vec<Workload> {
    vec![
        Workload {
            name: "probe-grep-exact-value",
            fixture: "probe_grep",
            prompt: "Search the codebase for `deadline`. Report the exact integer value \
                     assigned to the CHECKOUT_DEADLINE_MS constant. Answer with only that \
                     integer.",
            script: script_probe_grep,
            check: check_probe_grep_value,
            approval: ApprovalProfile::DenyGateNoPrompts,
            needles: &["47231"],
            build: None,
            require_failing_then_passing_bash: false,
        },
        Workload {
            // find compaction (issue-340): the target lives in a >1000-file
            // tree that compacts. The question is phrased so the model CANNOT
            // route around the reduction with a targeted glob -- you cannot glob
            // for "the odd name out", so it must list broadly (`*.rs` /
            // `handler_*.rs` -> 1351 matches -> compaction) and scan the reduced
            // listing for the one non-numeric handler. The target sorts into the
            // shown prefix (newest mtime) and the render probe proves it
            // survives, so a green render probe guarantees the answer is present.
            name: "probe-find-odd-handler",
            fixture: "probe_find",
            prompt: "Every handler file is named `handler_NN_NN.rs` (two two-digit numbers) \
                     except exactly one. Using the find tool, list the handler files and \
                     report the full path of the single handler whose name does NOT follow \
                     that numeric pattern. Answer with only that path.",
            script: script_probe_find,
            check: check_probe_find_path,
            approval: ApprovalProfile::DenyGateNoPrompts,
            needles: &["handler_zebra_target.rs"],
            build: Some(build_find_tree),
            require_failing_then_passing_bash: false,
        },
        Workload {
            // read skim (issue-337): the answer is a body-level local inside the
            // `sweep` function, NOT a top-level symbol -- so the model must read
            // the code (a grep for a constant cannot answer it), which is what
            // exercises skim. skim keeps the function body verbatim while
            // stripping the heavy comment narrative. Scripted replay reads with
            // `skim:true`; the live model chooses its own path.
            name: "probe-read-sweep-local",
            fixture: "probe_read",
            prompt: "Read settlement.rs. Inside the `sweep` function body, what is the name \
                     of the local Vec variable that collects the due charge ids and is \
                     returned? Answer with only that identifier.",
            script: script_probe_read,
            check: check_probe_read_local,
            approval: ApprovalProfile::DenyGateNoPrompts,
            needles: &["due_ids"],
            build: None,
            require_failing_then_passing_bash: false,
        },
    ]
}

// -- scripted tool-call sequences -------------------------------------------

fn call_turn(id: &str, name: &str, arguments: Value) -> AssistantTurn {
    AssistantTurn {
        text: None,
        reasoning: Vec::new(),
        tool_calls: vec![ToolCall {
            id: id.to_string(),
            thought_signature: None,
            name: name.to_string(),
            arguments,
        }],
        response_id: None,
        usage: None,
        completion_reason: Some(CompletionReason::ToolUse),
    }
}

fn answer_turn(text: &str) -> AssistantTurn {
    AssistantTurn::text(text)
}

fn script_fix_test() -> Vec<AssistantTurn> {
    vec![
        call_turn("c1", "grep", json!({ "pattern": "parse_len" })),
        call_turn("c2", "read", json!({ "path": "parser.rs" })),
        call_turn(
            "c3",
            "edit",
            json!({
                "file_path": "parser.rs",
                "old_string": "s.split_whitespace().count() - 1",
                "new_string": "s.split_whitespace().count()",
            }),
        ),
        answer_turn(
            "Fixed the off-by-one in parser::parse_len -- removed the trailing `- 1` so it counts every whitespace-separated token.",
        ),
    ]
}

fn script_rename() -> Vec<AssistantTurn> {
    let files = [
        "config/retry.rs",
        "net/client.rs",
        "net/pool.rs",
        "worker/runner.rs",
        "docs/notes.md",
    ];
    let mut turns = vec![call_turn("g", "grep", json!({ "pattern": "MAX_RETRIES" }))];
    for (idx, file) in files.iter().enumerate() {
        turns.push(call_turn(
            &format!("r{idx}"),
            "read",
            json!({ "path": file }),
        ));
        turns.push(call_turn(
            &format!("e{idx}"),
            "edit",
            json!({
                "file_path": file,
                "old_string": "MAX_RETRIES",
                "new_string": "MAX_ATTEMPTS",
                "replace_all": true,
            }),
        ));
    }
    turns.push(answer_turn(
        "Renamed MAX_RETRIES to MAX_ATTEMPTS across config/retry.rs, net/client.rs, net/pool.rs, worker/runner.rs, and docs/notes.md.",
    ));
    turns
}

/// Scripted (deterministic) bash flow: read the buggy source in the confined
/// workspace and exit non-zero. The exact command is cheap on purpose -- the
/// deterministic gate test only proves the skip-permissions WIRING (gate
/// bypassed, bash executed, non-zero exit captured); the real model chooses its
/// own `cargo test` command in the live smoke.
fn script_bash_diagnose() -> Vec<AssistantTurn> {
    vec![
        call_turn("b", "bash", json!({ "command": "cat src/lib.rs; exit 3" })),
        answer_turn("The failing test is ceiling_is_exact: it asserted left: 8191, right: 8192."),
    ]
}

/// Scripted chained repair flow for the PR-404-style fixture: discover files,
/// inspect the noisy failing cargo test, patch the provider request, and verify
/// green. The real model chooses its own calls; this replay proves the tool
/// chain and output reducers preserve the facts the task needs.
fn script_chained_openai_summary_fix() -> Vec<AssistantTurn> {
    vec![
        call_turn("f", "find", json!({ "pattern": "*.rs" })),
        call_turn(
            "g1",
            "grep",
            json!({ "pattern": "reasoning", "path": "src" }),
        ),
        call_turn("g2", "grep", json!({ "pattern": "summary", "path": "." })),
        call_turn(
            "r1",
            "read",
            json!({ "path": "tests/live_reasoning_summary.rs" }),
        ),
        call_turn("b1", "bash", json!({ "command": "cargo test" })),
        call_turn(
            "r2",
            "read",
            json!({ "path": "src/providers/openai_codex_responses.rs" }),
        ),
        call_turn(
            "e",
            "edit",
            json!({
                "file_path": "src/providers/openai_codex_responses.rs",
                "old_string": "    Some(CodexReasoning {\n        effort,\n        summary: None,\n    })",
                "new_string": "    Some(CodexReasoning {\n        effort,\n        summary: Some(\"auto\"),\n    })",
            }),
        ),
        call_turn("b2", "bash", json!({ "command": "cargo test" })),
        answer_turn(
            "Fixed the OpenAI Codex reasoning request by adding summary:auto whenever reasoning is enabled; cargo test now passes.",
        ),
    ]
}

fn script_chained_iris_recall_span_fix() -> Vec<AssistantTurn> {
    vec![
        call_turn("b1", "bash", json!({ "command": "cargo test" })),
        call_turn("f", "find", json!({ "pattern": "*.rs" })),
        call_turn("g", "grep", json!({ "pattern": "missing recall handle" })),
        call_turn("r", "read", json!({ "path": "src/recall.rs" })),
        call_turn(
            "e",
            "edit",
            json!({
                "file_path": "src/recall.rs",
                "old_string": "        let (start, end) = if let Some(handle) = handle {\n            match handle {\n                \"recent-parser\" => (\"00000001\", \"00000003\"),\n                _ => return Err(format!(\"unknown recall handle {handle}\")),\n            }\n        } else {\n            return Err(\"missing recall handle\".to_string());\n        };\n        let start = from.unwrap_or(start);\n        let end = to.unwrap_or(end);\n        let selected: Vec<&str> = self.turns\n            .iter()\n            .filter(|turn| turn.id >= start && turn.id <= end)\n            .map(|turn| turn.text)\n            .collect();\n        Ok(selected.join(\"\\n\"))",
                "new_string": "        let (default_start, default_end) = if let Some(handle) = handle {\n            match handle {\n                \"recent-parser\" => (\"00000001\", \"00000003\"),\n                _ => return Err(format!(\"unknown recall handle {handle}\")),\n            }\n        } else {\n            (from.ok_or_else(|| \"missing recall span start\".to_string())?,\n             to.ok_or_else(|| \"missing recall span end\".to_string())?)\n        };\n        let start = from.unwrap_or(default_start);\n        let end = to.unwrap_or(default_end);\n        let selected: Vec<&str> = self.turns\n            .iter()\n            .filter(|turn| turn.id >= start && turn.id <= end)\n            .map(|turn| turn.text)\n            .collect();\n        if selected.is_empty() {\n            return Err(\"selected no turns in explicit span\".to_string());\n        }\n        Ok(selected.join(\"\\n\"))",
            }),
        ),
        call_turn("b2", "bash", json!({ "command": "cargo test" })),
        answer_turn(
            "Fixed recall standalone spans and out-of-range span errors; cargo test passes.",
        ),
    ]
}

fn script_chained_iris_fold_resume_fix() -> Vec<AssistantTurn> {
    vec![
        call_turn("b1", "bash", json!({ "command": "cargo test" })),
        call_turn("f", "find", json!({ "pattern": "*.rs" })),
        call_turn("g", "grep", json!({ "pattern": "modelSelection" })),
        call_turn("r", "read", json!({ "path": "src/session.rs" })),
        call_turn(
            "e",
            "edit",
            json!({
                "file_path": "src/session.rs",
                "old_string": "\"message\" | \"compaction\" | \"modelSelection\" =>",
                "new_string": "\"message\" | \"compaction\" | \"modelSelection\" | \"fold\" =>",
            }),
        ),
        call_turn("b2", "bash", json!({ "command": "cargo test" })),
        answer_turn(
            "Fixed resume scanning so fold entries remain in the durable id chain; cargo test passes.",
        ),
    ]
}

fn script_chained_ampi_github_token_fix() -> Vec<AssistantTurn> {
    vec![
        call_turn("b1", "bash", json!({ "command": "npm test" })),
        call_turn("f", "find", json!({ "pattern": "*.mjs" })),
        call_turn("g", "grep", json!({ "pattern": "AMPI_GITHUB_TOKEN" })),
        call_turn("r", "read", json!({ "path": "src/auth.mjs" })),
        call_turn(
            "e",
            "edit",
            json!({
                "file_path": "src/auth.mjs",
                "old_string": "  return env.AMPI_GITHUB_TOKEN ?? env.GITHUB_TOKEN ?? null;",
                "new_string": "  return env.AMPI_GITHUB_TOKEN ?? env.GITHUB_TOKEN ?? env.GH_TOKEN ?? env.GITHUB_PERSONAL_ACCESS_TOKEN ?? null;",
            }),
        ),
        call_turn("b2", "bash", json!({ "command": "npm test" })),
        answer_turn("Fixed GitHub token fallback handling; npm test passes."),
    ]
}

fn script_chained_ampi_pack_untracked_fix() -> Vec<AssistantTurn> {
    vec![
        call_turn("b1", "bash", json!({ "command": "npm test" })),
        call_turn("f", "find", json!({ "pattern": "*.mjs" })),
        call_turn("g", "grep", json!({ "pattern": "untracked" })),
        call_turn("r", "read", json!({ "path": "src/verify-pack.mjs" })),
        call_turn(
            "e",
            "edit",
            json!({
                "file_path": "src/verify-pack.mjs",
                "old_string": "  if (privateFiles.length > 0) {\n    throw new Error(`private files in package: ${privateFiles.map((f) => f.path).join(', ')}`);\n  }\n  return true;",
                "new_string": "  if (privateFiles.length > 0) {\n    throw new Error(`private files in package: ${privateFiles.map((f) => f.path).join(', ')}`);\n  }\n  const untrackedFiles = files.filter((file) => file.untracked === true);\n  if (untrackedFiles.length > 0) {\n    throw new Error(`untracked files in package: ${untrackedFiles.map((f) => f.path).join(', ')}`);\n  }\n  return true;",
            }),
        ),
        call_turn("b2", "bash", json!({ "command": "npm test" })),
        answer_turn("Fixed pack verification to reject untracked tarball files; npm test passes."),
    ]
}

fn script_chained_ampi_private_docs_fix() -> Vec<AssistantTurn> {
    vec![
        call_turn("b1", "bash", json!({ "command": "npm test" })),
        call_turn("f", "find", json!({ "pattern": "*.mjs" })),
        call_turn("g", "grep", json!({ "pattern": "docs/private" })),
        call_turn("r", "read", json!({ "path": "src/pack-files.mjs" })),
        call_turn(
            "e",
            "edit",
            json!({
                "file_path": "src/pack-files.mjs",
                "old_string": "  return files.filter((file) => !file.startsWith('.'));",
                "new_string": "  return files.filter((file) => !file.startsWith('.') && !file.startsWith('docs/private/'));",
            }),
        ),
        call_turn("b2", "bash", json!({ "command": "npm test" })),
        answer_turn("Fixed package file filtering to exclude docs/private; npm test passes."),
    ]
}

/// Scripted grep probe: search `deadline`, then answer the planted value.
fn script_probe_grep() -> Vec<AssistantTurn> {
    vec![
        call_turn(
            "g",
            "grep",
            json!({ "pattern": "deadline", "ignoreCase": true }),
        ),
        answer_turn("CHECKOUT_DEADLINE_MS is 47231."),
    ]
}

/// Scripted find probe: broad `*.rs` listing (trips compaction), then answer
/// the one non-numeric handler's path from the reduced listing.
fn script_probe_find() -> Vec<AssistantTurn> {
    vec![
        call_turn("f", "find", json!({ "pattern": "*.rs" })),
        answer_turn("services/aaa_target/gateway/handler_zebra_target.rs"),
    ]
}

/// Scripted read-skim probe: skim-read the comment-heavy source, then answer
/// the body-level local inside `sweep` (which survives skim as code).
fn script_probe_read() -> Vec<AssistantTurn> {
    vec![
        call_turn(
            "r",
            "read",
            json!({ "path": "settlement.rs", "skim": true }),
        ),
        answer_turn("due_ids"),
    ]
}

fn script_log_triage() -> Vec<AssistantTurn> {
    vec![
        call_turn(
            "g",
            "grep",
            json!({ "pattern": "assertion", "path": "logs" }),
        ),
        call_turn("r", "read", json!({ "path": "logs/shard-03.log" })),
        answer_turn(
            "The failing test is caps::tests::ceiling_is_exact (logs/shard-03.log): the token \
             budget ceiling drifted by one -- it reported left: 8192, right: 8191.",
        ),
    ]
}

// -- mechanical success checks ----------------------------------------------

/// Workload 1: the test goes green. Compiles the fixture crate with
/// `rustc --test` and runs it; success = every test passes (exit 0).
fn check_fix_test(workspace: &Path, _final_text: &str) -> Outcome {
    let bin = workspace.join("wl1_test_bin");
    let compile = Command::new("rustc")
        .args(["--test", "--edition", "2021", "-A", "warnings", "-o"])
        .arg(&bin)
        .arg(workspace.join("lib.rs"))
        .output();
    let compile = match compile {
        Ok(output) => output,
        Err(error) => {
            return Outcome {
                success: false,
                detail: format!("rustc not runnable: {error}"),
            };
        }
    };
    if !compile.status.success() {
        return Outcome {
            success: false,
            detail: format!(
                "fixture did not compile: {}",
                String::from_utf8_lossy(&compile.stderr).trim()
            ),
        };
    }
    match Command::new(&bin).output() {
        Ok(run) if run.status.success() => Outcome {
            success: true,
            detail: "cargo/rustc test binary exited 0 (all tests passed)".to_string(),
        },
        Ok(run) => Outcome {
            success: false,
            detail: format!(
                "test binary failed: {}",
                String::from_utf8_lossy(&run.stdout).trim()
            ),
        },
        Err(error) => Outcome {
            success: false,
            detail: format!("test binary not runnable: {error}"),
        },
    }
}

/// Workload 2: the expected diff is applied. No file may still contain the old
/// identifier, and every source that had it now has the new one.
fn check_rename(workspace: &Path, _final_text: &str) -> Outcome {
    let mut stray = Vec::new();
    let mut renamed = 0usize;
    for path in walk_files(workspace) {
        let content = fs::read_to_string(&path).unwrap_or_default();
        let rel = path.strip_prefix(workspace).unwrap_or(&path).display();
        if content.contains("MAX_RETRIES") {
            stray.push(rel.to_string());
        }
        if content.contains("MAX_ATTEMPTS") {
            renamed += 1;
        }
    }
    if stray.is_empty() && renamed >= 5 {
        Outcome {
            success: true,
            detail: format!("all occurrences renamed across {renamed} files, none left"),
        }
    } else {
        Outcome {
            success: false,
            detail: format!("renamed {renamed} files; stray MAX_RETRIES in {stray:?}"),
        }
    }
}

/// Workload 3: the planted fact is found. The answer must carry both the
/// planted left/right values (unique to shard-03), so a generic answer fails.
fn check_log_triage(_workspace: &Path, final_text: &str) -> Outcome {
    let has_left = final_text.contains("8192");
    let has_right = final_text.contains("8191");
    if has_left && has_right {
        Outcome {
            success: true,
            detail: "answer carries the planted left/right values (8192/8191)".to_string(),
        }
    } else {
        Outcome {
            success: false,
            detail: format!("answer missing planted values (8192={has_left}, 8191={has_right})"),
        }
    }
}

/// Bash diagnosis: the model's answer must carry both planted assertion values
/// (unique to this crate's failing test), so a generic answer fails.
fn check_bash_diagnose(_workspace: &Path, final_text: &str) -> Outcome {
    let has_left = final_text.contains("8191");
    let has_right = final_text.contains("8192");
    if has_left && has_right {
        Outcome {
            success: true,
            detail: "answer carries both planted assertion values (8191/8192)".to_string(),
        }
    } else {
        Outcome {
            success: false,
            detail: format!("answer missing planted values (8191={has_left}, 8192={has_right})"),
        }
    }
}

/// Chained bash repair: the OpenAI Codex request builder must request
/// `summary: "auto"` when reasoning is enabled, and the fixture's cargo tests
/// must pass after the agent's edit. This mirrors PR #404's root cause while
/// keeping the success check mechanical and outside the model.
fn check_chained_openai_summary_fix(workspace: &Path, _final_text: &str) -> Outcome {
    let provider_path = workspace.join("src/providers/openai_codex_responses.rs");
    let source = fs::read_to_string(&provider_path).unwrap_or_default();
    let has_summary_auto = source.contains("summary: Some(\"auto\")");
    let run = Command::new("cargo")
        .arg("test")
        .current_dir(workspace)
        .output();
    match run {
        Ok(output) if output.status.success() && has_summary_auto => Outcome {
            success: true,
            detail: "cargo test passed and the Codex reasoning request includes summary:auto"
                .to_string(),
        },
        Ok(output) => Outcome {
            success: false,
            detail: format!(
                "summary:auto present={has_summary_auto}; cargo test status={}; stdout={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        },
        Err(error) => Outcome {
            success: false,
            detail: format!("cargo test not runnable: {error}"),
        },
    }
}

fn command_success(workspace: &Path, program: &str, args: &[&str]) -> Result<(), String> {
    match Command::new(program)
        .args(args)
        .current_dir(workspace)
        .output()
    {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!(
            "{program} {} status={}; stdout={} stderr={}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(error) => Err(format!("{program} not runnable: {error}")),
    }
}

fn check_chained_iris_recall_span_fix(workspace: &Path, _final_text: &str) -> Outcome {
    let tests = fs::read_to_string(workspace.join("tests/recall_span.rs")).unwrap_or_default();
    let tests_still_assert_span = tests
        .contains("standalone_span_without_handle_returns_original_turns")
        && tests.contains("out_of_range_standalone_span_is_an_error")
        && tests.contains("expect_err(\"empty explicit spans must be tool errors\")")
        && tests.contains("selected no turns");
    match command_success(workspace, "cargo", &["test"]) {
        Ok(()) if tests_still_assert_span => Outcome {
            success: true,
            detail: "cargo test passed with standalone-span and empty-span tests intact"
                .to_string(),
        },
        Ok(()) => Outcome {
            success: false,
            detail: "cargo test passed but recall span tests were changed or weakened".to_string(),
        },
        Err(detail) => Outcome {
            success: false,
            detail,
        },
    }
}

fn check_chained_iris_fold_resume_fix(workspace: &Path, _final_text: &str) -> Outcome {
    let source = fs::read_to_string(workspace.join("src/session.rs")).unwrap_or_default();
    let has_fold = source.contains("\"modelSelection\" | \"fold\"")
        || source.contains("\"fold\" | \"modelSelection\"");
    match command_success(workspace, "cargo", &["test"]) {
        Ok(()) if has_fold => Outcome {
            success: true,
            detail: "cargo test passed; fold is counted in resume chain".to_string(),
        },
        Ok(()) => Outcome {
            success: false,
            detail: "cargo test passed but fold branch is missing from resume scanner".to_string(),
        },
        Err(detail) => Outcome {
            success: false,
            detail,
        },
    }
}

fn check_chained_ampi_github_token_fix(workspace: &Path, _final_text: &str) -> Outcome {
    let source = fs::read_to_string(workspace.join("src/auth.mjs")).unwrap_or_default();
    let has_gh = source.contains("GH_TOKEN");
    let has_pat = source.contains("GITHUB_PERSONAL_ACCESS_TOKEN");
    match command_success(workspace, "npm", &["test"]) {
        Ok(()) if has_gh && has_pat => Outcome {
            success: true,
            detail: "npm test passed; GitHub token fallbacks implemented".to_string(),
        },
        Ok(()) => Outcome {
            success: false,
            detail: format!(
                "npm test passed but fallback code missing (GH={has_gh}, PAT={has_pat})"
            ),
        },
        Err(detail) => Outcome {
            success: false,
            detail,
        },
    }
}

fn check_chained_ampi_pack_untracked_fix(workspace: &Path, _final_text: &str) -> Outcome {
    let source = fs::read_to_string(workspace.join("src/verify-pack.mjs")).unwrap_or_default();
    let has_untracked =
        source.contains("untrackedFiles") && source.contains("untracked files in package");
    match command_success(workspace, "npm", &["test"]) {
        Ok(()) if has_untracked => Outcome {
            success: true,
            detail: "npm test passed; untracked tarball files are rejected".to_string(),
        },
        Ok(()) => Outcome {
            success: false,
            detail: "npm test passed but untracked-file guard is missing".to_string(),
        },
        Err(detail) => Outcome {
            success: false,
            detail,
        },
    }
}

fn check_chained_ampi_private_docs_fix(workspace: &Path, _final_text: &str) -> Outcome {
    let source = fs::read_to_string(workspace.join("src/pack-files.mjs")).unwrap_or_default();
    let excludes_private = source.contains("docs/private/");
    match command_success(workspace, "npm", &["test"]) {
        Ok(()) if excludes_private => Outcome {
            success: true,
            detail: "npm test passed; docs/private is excluded".to_string(),
        },
        Ok(()) => Outcome {
            success: false,
            detail: "npm test passed but docs/private exclusion is missing".to_string(),
        },
        Err(detail) => Outcome {
            success: false,
            detail,
        },
    }
}

/// Grep exact-value probe: the answer must carry the planted constant value.
fn check_probe_grep_value(_workspace: &Path, final_text: &str) -> Outcome {
    if final_text.contains("47231") {
        Outcome {
            success: true,
            detail: "answer carries the exact CHECKOUT_DEADLINE_MS value (47231)".to_string(),
        }
    } else {
        Outcome {
            success: false,
            detail: "answer missing the exact value 47231".to_string(),
        }
    }
}

/// Read-skim probe: the answer must name the body-level local inside `sweep`,
/// which the model can only get by reading the code -- skim keeps the function
/// body verbatim while stripping the surrounding comment narrative.
fn check_probe_read_local(_workspace: &Path, final_text: &str) -> Outcome {
    if final_text.contains("due_ids") {
        Outcome {
            success: true,
            detail: "answer names the sweep body-local variable (due_ids)".to_string(),
        }
    } else {
        Outcome {
            success: false,
            detail: "answer missing the body-local identifier due_ids".to_string(),
        }
    }
}

/// Find target-path probe: the answer must name the distinctive target file the
/// grouped listing had to surface from a >1000-file compacted tree.
fn check_probe_find_path(_workspace: &Path, final_text: &str) -> Outcome {
    if final_text.contains("handler_zebra_target.rs") {
        Outcome {
            success: true,
            detail: "answer names the target path (handler_zebra_target.rs)".to_string(),
        }
    } else {
        Outcome {
            success: false,
            detail: "answer missing the target file handler_zebra_target.rs".to_string(),
        }
    }
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}
