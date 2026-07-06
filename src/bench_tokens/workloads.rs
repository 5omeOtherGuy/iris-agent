//! Workload definitions: the fixture, the prompt, the scripted replay
//! sequence, the mechanical (harness-side, no-shell) success check, and the
//! verbatim needles the tool output must surface in both arms.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::nexus::{AssistantTurn, CompletionReason, ToolCall};

use super::fixtures::{build_chained_all_tree, build_find_tree, build_repair_noise_tree};

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
    /// Marks a repair-loop bash workload: the fixture ships broken and the task
    /// is to make its tests pass. The harness brackets the run by confirming the
    /// fixture is genuinely failing BEFORE the model runs (see
    /// `fixture_starts_broken`) and letting the post-run mechanical check decide
    /// success. Validity is thus independent of how the model plumbs its test
    /// command -- e.g. piping `cargo test` through `head` masks the failing exit
    /// code, so gating on the recorded exit sequence measured shell style, not
    /// the repair. `false` for read-only or non-repair workloads.
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
            name: "chained-clap-conflict-panic-fix",
            fixture: "workload_clap_conflict_panic",
            prompt: "A command panics instead of reporting an argument conflict when a \
                     conflict rule names an argument group. This fixture mirrors clap \
                     PR #5298. First, before reading or editing any file, run `cargo test` \
                     to reproduce the failure. Then use find/grep/read to locate the \
                     conflict-reporting code, fix it without weakening tests, and run \
                     `cargo test` until it passes. Summarize the fix.",
            script: script_chained_clap_conflict_panic_fix,
            check: check_chained_clap_conflict_panic_fix,
            approval: ApprovalProfile::SkipPermissions,
            needles: &["self.find(id).unwrap()", "used_arg_context"],
            build: Some(build_repair_noise_tree),
            require_failing_then_passing_bash: true,
        },
        Workload {
            name: "chained-bytes-sign-extend-fix",
            fixture: "workload_bytes_sign_extend",
            prompt: "Signed variable-width integer reads decode negative values as large \
                     positive numbers. This fixture mirrors tokio-rs/bytes PR #732. First, \
                     before reading or editing any file, run `cargo test` to reproduce the \
                     failure. Then use find/grep/read to locate the integer readers, fix \
                     them without weakening tests, and run `cargo test` until it passes. \
                     Summarize the fix.",
            script: script_chained_bytes_sign_extend_fix,
            check: check_chained_bytes_sign_extend_fix,
            approval: ApprovalProfile::SkipPermissions,
            needles: &["i64::from_be_bytes(buf)", "i64::from_le_bytes(buf)"],
            build: Some(build_repair_noise_tree),
            require_failing_then_passing_bash: true,
        },
        Workload {
            name: "chained-nushell-not-precedence-fix",
            fixture: "workload_nushell_not_precedence",
            prompt: "The prefix `not` operator has the wrong precedence: `not false and \
                     false` evaluates to true instead of false. This fixture mirrors \
                     nushell PR #11672. First, before reading or editing any file, run \
                     `cargo test` to reproduce the failure. Then use find/grep/read to \
                     locate the expression parser, fix it without weakening tests, and run \
                     `cargo test` until it passes. Summarize the fix.",
            script: script_chained_nushell_not_precedence_fix,
            check: check_chained_nushell_not_precedence_fix,
            approval: ApprovalProfile::SkipPermissions,
            needles: &["self.parse_expr(0)", "NOT_BINDING_POWER"],
            build: Some(build_repair_noise_tree),
            require_failing_then_passing_bash: true,
        },
        Workload {
            name: "chained-dayjs-tz-locale-fix",
            fixture: "workload_dayjs_tz_locale",
            prompt: "The timezone reconstruction drops the instance locale, so \
                     startOf('week') ignores the configured weekStart. This fixture mirrors \
                     iamkun/dayjs PR #2420. First, before reading or editing any file, run \
                     `npm test` to reproduce the failure. Then use find/grep/read to locate \
                     the timezone reconstruction, fix it without weakening tests, and run \
                     `npm test` until it passes. Summarize the fix.",
            script: script_chained_dayjs_tz_locale_fix,
            check: check_chained_dayjs_tz_locale_fix,
            approval: ApprovalProfile::SkipPermissions,
            needles: &["d(target, offsetMinutes)", "weekStart"],
            build: Some(build_repair_noise_tree),
            require_failing_then_passing_bash: true,
        },
        Workload {
            name: "chained-all-four-fix",
            fixture: "workload_chained_all",
            prompt: "This workspace holds four independent bug fixtures in subdirectories: \
                     `bytes/`, `clap/`, `nushell/`, `dayjs/`. Fix all four, IN THIS ORDER: \
                     (1) bytes, (2) clap, (3) nushell, (4) dayjs. For each, first reproduce \
                     the failure by running its tests, then use find/grep/read to locate the \
                     bug, fix it without weakening tests, and re-run its tests until they pass \
                     before moving to the next. Run the Rust subprojects with \
                     `cargo test --manifest-path <sub>/Cargo.toml` and dayjs with \
                     `npm test --prefix dayjs`. The bugs: bytes -- signed variable-width int \
                     reads decode negatives as large positives (need sign extension); clap -- \
                     a conflict rule naming an argument group panics instead of erroring; \
                     nushell -- the prefix `not` operator has the wrong precedence \
                     (`not false and false` is true); dayjs -- timezone reconstruction drops \
                     the instance locale so startOf('week') ignores weekStart. When all four \
                     pass, summarize each fix.",
            script: script_chained_all_fix,
            check: check_chained_all_fix,
            approval: ApprovalProfile::SkipPermissions,
            needles: &[
                "i64::from_be_bytes(buf)",
                "self.find(id).unwrap()",
                "self.parse_expr(0)",
                "d(target, offsetMinutes)",
            ],
            build: Some(build_chained_all_tree),
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
fn script_chained_clap_conflict_panic_fix() -> Vec<AssistantTurn> {
    vec![
        call_turn("b1", "bash", json!({ "command": "cargo test" })),
        call_turn("f", "find", json!({ "pattern": "*.rs" })),
        call_turn("g", "grep", json!({ "pattern": "unwrap", "path": "src" })),
        call_turn("r", "read", json!({ "path": "src/lib.rs" })),
        call_turn(
            "e",
            "edit",
            json!({
                "file_path": "src/lib.rs",
                "old_string": "            .map(|id| self.find(id).unwrap().to_string())",
                "new_string": "            .filter_map(|id| self.find(id).map(|a| a.to_string()))",
            }),
        ),
        call_turn("b2", "bash", json!({ "command": "cargo test" })),
        answer_turn(
            "Fixed the conflict-context builder to skip non-argument ids (group ids) instead of unwrapping; cargo test passes.",
        ),
    ]
}

fn script_chained_bytes_sign_extend_fix() -> Vec<AssistantTurn> {
    vec![
        call_turn("b1", "bash", json!({ "command": "cargo test" })),
        call_turn("f", "find", json!({ "pattern": "*.rs" })),
        call_turn(
            "g",
            "grep",
            json!({ "pattern": "from_be_bytes", "path": "src" }),
        ),
        call_turn("r", "read", json!({ "path": "src/buf_impl.rs" })),
        call_turn(
            "e1",
            "edit",
            json!({
                "file_path": "src/buf_impl.rs",
                "old_string": "        let mut buf = [0u8; 8];\n        // Big-endian: copy nbytes into the low-order positions.\n        self.copy_to_slice(&mut buf[8 - nbytes..]);\n        i64::from_be_bytes(buf)",
                "new_string": "        let mut buf = [0u8; 8];\n        // Big-endian: copy nbytes into the low-order positions.\n        self.copy_to_slice(&mut buf[8 - nbytes..]);\n        let shift = (8 - nbytes) * 8;\n        ((u64::from_be_bytes(buf) << shift) as i64) >> shift",
            }),
        ),
        call_turn(
            "e2",
            "edit",
            json!({
                "file_path": "src/buf_impl.rs",
                "old_string": "        let mut buf = [0u8; 8];\n        // Little-endian: copy nbytes into the low-order positions.\n        self.copy_to_slice(&mut buf[..nbytes]);\n        i64::from_le_bytes(buf)",
                "new_string": "        let mut buf = [0u8; 8];\n        // Little-endian: copy nbytes into the low-order positions.\n        self.copy_to_slice(&mut buf[..nbytes]);\n        let shift = (8 - nbytes) * 8;\n        ((u64::from_le_bytes(buf) << shift) as i64) >> shift",
            }),
        ),
        call_turn("b2", "bash", json!({ "command": "cargo test" })),
        answer_turn(
            "Fixed signed integer decoding to sign-extend narrow values instead of zero-padding; cargo test passes.",
        ),
    ]
}

fn script_chained_nushell_not_precedence_fix() -> Vec<AssistantTurn> {
    vec![
        call_turn("b1", "bash", json!({ "command": "cargo test" })),
        call_turn("f", "find", json!({ "pattern": "*.rs" })),
        call_turn(
            "g",
            "grep",
            json!({ "pattern": "parse_expr", "path": "src" }),
        ),
        call_turn("r", "read", json!({ "path": "src/lib.rs" })),
        call_turn(
            "e",
            "edit",
            json!({
                "file_path": "src/lib.rs",
                "old_string": "            let operand = self.parse_expr(0)?;",
                "new_string": "            let operand = self.parse_expr(NOT_BINDING_POWER)?;",
            }),
        ),
        call_turn("b2", "bash", json!({ "command": "cargo test" })),
        answer_turn(
            "Fixed `not` precedence so it binds only to the next value; cargo test passes.",
        ),
    ]
}

fn script_chained_dayjs_tz_locale_fix() -> Vec<AssistantTurn> {
    vec![
        call_turn("b1", "bash", json!({ "command": "npm test" })),
        call_turn("f", "find", json!({ "pattern": "*.mjs" })),
        call_turn("g", "grep", json!({ "pattern": "rebuilt", "path": "src" })),
        call_turn("r", "read", json!({ "path": "src/index.mjs" })),
        call_turn(
            "e",
            "edit",
            json!({
                "file_path": "src/index.mjs",
                "old_string": "    const rebuilt = d(target, offsetMinutes);",
                "new_string": "    const rebuilt = d(target, offsetMinutes).locale(this.$L);",
            }),
        ),
        call_turn("b2", "bash", json!({ "command": "npm test" })),
        answer_turn(
            "Fixed the timezone reconstruction to preserve the instance locale (weekStart); npm test passes.",
        ),
    ]
}

/// Combined workload: fix all four bugs in order (bytes, clap, nushell, dayjs),
/// each in its own subdirectory, reproducing then repairing then re-verifying
/// before the next. Subproject tests run via `--manifest-path` / `--prefix` so
/// no `cd`/chaining is needed. Produces a genuine failing-then-passing sequence
/// per subproject.
fn script_chained_all_fix() -> Vec<AssistantTurn> {
    vec![
        // (1) bytes
        call_turn(
            "by1",
            "bash",
            json!({ "command": "cargo test --manifest-path bytes/Cargo.toml" }),
        ),
        call_turn(
            "byg",
            "grep",
            json!({ "pattern": "from_be_bytes", "path": "bytes/src" }),
        ),
        call_turn("byr", "read", json!({ "path": "bytes/src/buf_impl.rs" })),
        call_turn(
            "bye1",
            "edit",
            json!({
                "file_path": "bytes/src/buf_impl.rs",
                "old_string": "        let mut buf = [0u8; 8];\n        // Big-endian: copy nbytes into the low-order positions.\n        self.copy_to_slice(&mut buf[8 - nbytes..]);\n        i64::from_be_bytes(buf)",
                "new_string": "        let mut buf = [0u8; 8];\n        // Big-endian: copy nbytes into the low-order positions.\n        self.copy_to_slice(&mut buf[8 - nbytes..]);\n        let shift = (8 - nbytes) * 8;\n        ((u64::from_be_bytes(buf) << shift) as i64) >> shift",
            }),
        ),
        call_turn(
            "bye2",
            "edit",
            json!({
                "file_path": "bytes/src/buf_impl.rs",
                "old_string": "        let mut buf = [0u8; 8];\n        // Little-endian: copy nbytes into the low-order positions.\n        self.copy_to_slice(&mut buf[..nbytes]);\n        i64::from_le_bytes(buf)",
                "new_string": "        let mut buf = [0u8; 8];\n        // Little-endian: copy nbytes into the low-order positions.\n        self.copy_to_slice(&mut buf[..nbytes]);\n        let shift = (8 - nbytes) * 8;\n        ((u64::from_le_bytes(buf) << shift) as i64) >> shift",
            }),
        ),
        call_turn(
            "by2",
            "bash",
            json!({ "command": "cargo test --manifest-path bytes/Cargo.toml" }),
        ),
        // (2) clap
        call_turn(
            "cl1",
            "bash",
            json!({ "command": "cargo test --manifest-path clap/Cargo.toml" }),
        ),
        call_turn(
            "clg",
            "grep",
            json!({ "pattern": "unwrap", "path": "clap/src" }),
        ),
        call_turn("clr", "read", json!({ "path": "clap/src/lib.rs" })),
        call_turn(
            "cle",
            "edit",
            json!({
                "file_path": "clap/src/lib.rs",
                "old_string": "            .map(|id| self.find(id).unwrap().to_string())",
                "new_string": "            .filter_map(|id| self.find(id).map(|a| a.to_string()))",
            }),
        ),
        call_turn(
            "cl2",
            "bash",
            json!({ "command": "cargo test --manifest-path clap/Cargo.toml" }),
        ),
        // (3) nushell
        call_turn(
            "nu1",
            "bash",
            json!({ "command": "cargo test --manifest-path nushell/Cargo.toml" }),
        ),
        call_turn(
            "nug",
            "grep",
            json!({ "pattern": "parse_expr", "path": "nushell/src" }),
        ),
        call_turn("nur", "read", json!({ "path": "nushell/src/lib.rs" })),
        call_turn(
            "nue",
            "edit",
            json!({
                "file_path": "nushell/src/lib.rs",
                "old_string": "            let operand = self.parse_expr(0)?;",
                "new_string": "            let operand = self.parse_expr(NOT_BINDING_POWER)?;",
            }),
        ),
        call_turn(
            "nu2",
            "bash",
            json!({ "command": "cargo test --manifest-path nushell/Cargo.toml" }),
        ),
        // (4) dayjs
        call_turn(
            "dj1",
            "bash",
            json!({ "command": "npm test --prefix dayjs" }),
        ),
        call_turn(
            "djg",
            "grep",
            json!({ "pattern": "rebuilt", "path": "dayjs/src" }),
        ),
        call_turn("djr", "read", json!({ "path": "dayjs/src/index.mjs" })),
        call_turn(
            "dje",
            "edit",
            json!({
                "file_path": "dayjs/src/index.mjs",
                "old_string": "    const rebuilt = d(target, offsetMinutes);",
                "new_string": "    const rebuilt = d(target, offsetMinutes).locale(this.$L);",
            }),
        ),
        call_turn(
            "dj2",
            "bash",
            json!({ "command": "npm test --prefix dayjs" }),
        ),
        answer_turn(
            "Fixed all four: bytes sign-extends signed int reads; clap skips group ids in the \
             conflict context; nushell binds `not` to the next value only; dayjs preserves the \
             instance locale across the tz rebuild. All subproject tests pass.",
        ),
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

/// Chained bash repair (clap PR #5298): the conflict-context builder must skip
/// non-argument ids (group ids) instead of unwrapping, so a group-vs-subcommand
/// conflict returns an error rather than panicking. Robust check: cargo test
/// passes AND the regression test is still intact (not weakened).
fn check_chained_clap_conflict_panic_fix(workspace: &Path, _final_text: &str) -> Outcome {
    let tests = fs::read_to_string(workspace.join("tests/conflicts.rs")).unwrap_or_default();
    let tests_intact = tests.contains("fn group_conflicts_with_subcommand")
        && tests.contains("ErrorKind::ArgumentConflict")
        && tests.contains("expect_err(");
    match command_success(workspace, "cargo", &["test"]) {
        Ok(()) if tests_intact => Outcome {
            success: true,
            detail: "cargo test passed with the group-conflict regression test intact".to_string(),
        },
        Ok(()) => Outcome {
            success: false,
            detail: "cargo test passed but the conflict regression test was changed or weakened"
                .to_string(),
        },
        Err(detail) => Outcome {
            success: false,
            detail,
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

fn check_chained_bytes_sign_extend_fix(workspace: &Path, _final_text: &str) -> Outcome {
    let tests = fs::read_to_string(workspace.join("tests/test_buf.rs")).unwrap_or_default();
    let tests_intact = tests.contains("fn test_get_int")
        && tests.contains("assert_eq!(-42, buf.get_int(1))")
        && tests.contains("buf.get_int_le(3)");
    match command_success(workspace, "cargo", &["test"]) {
        Ok(()) if tests_intact => Outcome {
            success: true,
            detail: "cargo test passed with the signed-int regression test intact".to_string(),
        },
        Ok(()) => Outcome {
            success: false,
            detail: "cargo test passed but the signed-int regression test was changed or weakened"
                .to_string(),
        },
        Err(detail) => Outcome {
            success: false,
            detail,
        },
    }
}

fn check_chained_nushell_not_precedence_fix(workspace: &Path, _final_text: &str) -> Outcome {
    let tests = fs::read_to_string(workspace.join("tests/precedence.rs")).unwrap_or_default();
    let tests_intact = tests.contains("fn not_precedence")
        && tests.contains("eval(\"not false and false\")")
        && tests.contains("assert_eq!");
    match command_success(workspace, "cargo", &["test"]) {
        Ok(()) if tests_intact => Outcome {
            success: true,
            detail: "cargo test passed with the not-precedence regression test intact".to_string(),
        },
        Ok(()) => Outcome {
            success: false,
            detail:
                "cargo test passed but the not-precedence regression test was changed or weakened"
                    .to_string(),
        },
        Err(detail) => Outcome {
            success: false,
            detail,
        },
    }
}

fn check_chained_dayjs_tz_locale_fix(workspace: &Path, _final_text: &str) -> Outcome {
    let tests = fs::read_to_string(workspace.join("tests/tz-locale.test.mjs")).unwrap_or_default();
    let tests_intact = tests.contains("preserves locality when tz is called")
        && tests.contains("2023-02-15")
        && tests.contains("weekStart: 3");
    match command_success(workspace, "npm", &["test"]) {
        Ok(()) if tests_intact => Outcome {
            success: true,
            detail: "npm test passed with the tz-locale regression test intact".to_string(),
        },
        Ok(()) => Outcome {
            success: false,
            detail: "npm test passed but the tz-locale regression test was changed or weakened"
                .to_string(),
        },
        Err(detail) => Outcome {
            success: false,
            detail,
        },
    }
}

/// Combined workload: every one of the four subprojects must pass its tests AND
/// keep its regression test intact. Fails on the first subproject that is broken
/// or weakened, so on a pristine workspace (bytes still buggy) this returns
/// unsuccessful -- which is exactly what `fixture_starts_broken` needs.
fn check_chained_all_fix(workspace: &Path, _final_text: &str) -> Outcome {
    let subprojects: [(&str, &str, &str, &[&str]); 4] = [
        (
            "bytes",
            "cargo",
            "tests/test_buf.rs",
            &[
                "fn test_get_int",
                "assert_eq!(-42, buf.get_int(1))",
                "buf.get_int_le(3)",
            ],
        ),
        (
            "clap",
            "cargo",
            "tests/conflicts.rs",
            &[
                "fn group_conflicts_with_subcommand",
                "ErrorKind::ArgumentConflict",
                "expect_err(",
            ],
        ),
        (
            "nushell",
            "cargo",
            "tests/precedence.rs",
            &[
                "fn not_precedence",
                "eval(\"not false and false\")",
                "assert_eq!",
            ],
        ),
        (
            "dayjs",
            "npm",
            "tests/tz-locale.test.mjs",
            &[
                "preserves locality when tz is called",
                "2023-02-15",
                "weekStart: 3",
            ],
        ),
    ];
    for (sub, program, test_file, needles) in subprojects {
        let dir = workspace.join(sub);
        let tests = fs::read_to_string(dir.join(test_file)).unwrap_or_default();
        if !needles.iter().all(|n| tests.contains(n)) {
            return Outcome {
                success: false,
                detail: format!("{sub}: regression test {test_file} changed, weakened, or missing"),
            };
        }
        if let Err(detail) = command_success(&dir, program, &["test"]) {
            return Outcome {
                success: false,
                detail: format!("{sub}: {detail}"),
            };
        }
    }
    Outcome {
        success: true,
        detail: "all four subprojects pass with their regression tests intact".to_string(),
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
