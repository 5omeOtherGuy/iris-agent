//! Real-run workload catalog. A script-free counterpart to the `#[cfg(test)]`
//! replay `Workload` in iris-agent: each entry carries the prompt, the fixture
//! id, a mechanical success check, and the approval/bash policy. Fixtures are
//! materialized by `crate::fixtures`.
//!
//! NOTE: this is the contract stub. The catalog + check/enforce bodies are
//! filled by the port (see `fixtures.rs` for the fixture builders).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Result of a workload's mechanical success check.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub success: bool,
    pub detail: String,
}

/// One real-run workload.
#[derive(Clone)]
pub struct WorkloadSpec {
    pub name: &'static str,
    pub fixture_id: &'static str,
    pub prompt: &'static str,
    /// Mechanical success check, run after the agent turn over the workspace
    /// and the final assistant text.
    pub check: fn(&Path, &str) -> Outcome,
    /// Whether this workload runs bash under skip-permissions.
    pub skip_permissions: bool,
    /// Facts the tool output must surface for the task to be solvable.
    pub needles: &'static [&'static str],
    /// Optional programmatic workspace builder (large trees not committed).
    pub build: Option<fn(&Path)>,
    /// Require a failing bash before the final passing bash (chained repair).
    pub require_failing_then_passing_bash: bool,
}

/// The full committed catalog. Filled by the port.
pub fn catalog() -> Vec<WorkloadSpec> {
    vec![
        WorkloadSpec {
            name: "fix-failing-test",
            fixture_id: "workload1_fix_test",
            prompt: "The test `parse_len_counts_all_tokens` fails. Find and fix the bug in \
                     parse_len using read/grep/find and edit only. Do not run any shell \
                     commands; the test will be run for you.",
            check: check_fix_test,
            skip_permissions: false,
            needles: &["parse_len", "split_whitespace().count() - 1"],
            build: None,
            require_failing_then_passing_bash: false,
        },
        WorkloadSpec {
            name: "multi-file-search-and-edit",
            fixture_id: "workload2_rename",
            prompt: "Rename the identifier MAX_RETRIES to MAX_ATTEMPTS everywhere it appears \
                     in this tree (code and docs). Use grep/find to locate every occurrence \
                     and edit to change them. Do not run any shell commands.",
            check: check_rename,
            skip_permissions: false,
            needles: &["MAX_RETRIES"],
            build: None,
            require_failing_then_passing_bash: false,
        },
        WorkloadSpec {
            name: "investigate-large-log",
            fixture_id: "workload3_log_triage",
            prompt: "One test failed with a token-budget ceiling assertion. Search the logs/ \
                     directory to find which test failed and the exact left/right values it \
                     reported. Answer in one sentence. Do not run any shell commands.",
            check: check_log_triage,
            skip_permissions: false,
            needles: &["ceiling_is_exact", "8192", "8191"],
            build: None,
            require_failing_then_passing_bash: false,
        },
        WorkloadSpec {
            name: "probe-grep-exact-value",
            fixture_id: "probe_grep",
            prompt: "Search the codebase for `deadline`. Report the exact integer value \
                     assigned to the CHECKOUT_DEADLINE_MS constant. Answer with only that \
                     integer.",
            check: check_probe_grep_value,
            skip_permissions: false,
            needles: &["47231"],
            build: None,
            require_failing_then_passing_bash: false,
        },
        WorkloadSpec {
            name: "probe-find-odd-handler",
            fixture_id: "probe_find",
            prompt: "Every handler file is named `handler_NN_NN.rs` (two two-digit numbers) \
                     except exactly one. Using the find tool, list the handler files and \
                     report the full path of the single handler whose name does NOT follow \
                     that numeric pattern. Answer with only that path.",
            check: check_probe_find_path,
            skip_permissions: false,
            needles: &["handler_zebra_target.rs"],
            build: Some(crate::fixtures::build_find_tree),
            require_failing_then_passing_bash: false,
        },
        WorkloadSpec {
            name: "probe-read-sweep-local",
            fixture_id: "probe_read",
            prompt: "Read settlement.rs. Inside the `sweep` function body, what is the name \
                     of the local Vec variable that collects the due charge ids and is \
                     returned? Answer with only that identifier.",
            check: check_probe_read_local,
            skip_permissions: false,
            needles: &["due_ids"],
            build: None,
            require_failing_then_passing_bash: false,
        },
        WorkloadSpec {
            name: "bash-diagnose-test-failure",
            fixture_id: "workload4_bash_diagnose",
            prompt: "The crate's tests fail. Run the tests, read the failure, and report which \
                     test failed and the exact left/right values it asserted. Answer in one \
                     sentence. Do not edit any files.",
            check: check_bash_diagnose,
            skip_permissions: true,
            needles: &["8191", "8192"],
            build: None,
            require_failing_then_passing_bash: false,
        },
        WorkloadSpec {
            name: "chained-openai-summary-fix",
            fixture_id: "workload5_chained_provider_fix",
            prompt: "Live reasoning summaries do not stream for the OpenAI Codex Responses \
                     provider even when reasoning effort is enabled. This fixture mirrors a \
                     recent Iris bug. First, before reading or editing any file, run \
                     `cargo test` to reproduce the failure. Then discover the relevant files \
                     with find/grep/read, fix the request builder without weakening tests, \
                     and run `cargo test` until it passes. Summarize the fix.",
            check: check_chained_openai_summary_fix,
            skip_permissions: true,
            needles: &[
                "reasoning_request_asks_for_summary_so_live_thinking_can_stream",
                "summary: None",
                "summary:auto",
            ],
            build: Some(crate::fixtures::build_chained_provider_tree),
            require_failing_then_passing_bash: true,
        },
    ]
}

/// Look up a workload by name.
pub fn by_name(name: &str) -> Option<WorkloadSpec> {
    catalog().into_iter().find(|w| w.name == name)
}

/// For chained-repair workloads, downgrade a success to failure unless the
/// bash exit codes show a failing test reproduced BEFORE the final passing one.
pub fn enforce_failing_then_passing_bash(
    spec: &WorkloadSpec,
    outcome: &mut Outcome,
    exits: &[i32],
) -> bool {
    if !spec.require_failing_then_passing_bash || !outcome.success {
        return true;
    }
    let Some((&last, before_last)) = exits.split_last() else {
        outcome.success = false;
        outcome.detail = "expected a failing cargo test before the final passing cargo test; no bash exits were recorded".to_string();
        return false;
    };
    let reproduced_failure = before_last.iter().any(|&code| code != 0);
    if last != 0 || !reproduced_failure {
        outcome.success = false;
        outcome.detail = format!(
            "expected failing-then-passing bash exits for the chained repair; got {exits:?}"
        );
        return false;
    }
    true
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
