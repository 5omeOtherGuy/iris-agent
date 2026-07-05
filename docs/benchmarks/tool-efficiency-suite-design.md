# Tool-efficiency benchmark suite - design

Status: proposed (awaiting operator sign-off on phasing). Supersedes the ad-hoc
`bench_tokens_per_task.rs` layout; keeps its proven parts. Owner issue: #210.

## Purpose

Prove or disprove, per tool and end-to-end, that Iris's token-efficiency
reductions (ADR-0036/0037) cut tokens WITHOUT cutting task success. The existing
per-tool benches (`docs/benchmarks/issue-33x`, `adr-0037`) measure byte
reduction on fixtures only. They never check whether a real model still succeeds
with the reduced output. This suite closes that gap: real-model probes scored by
mechanical checks, on both cost and success, with negative/inconclusive results
reported as first-class.

## What each arm actually toggles

Verified against the code (Phase 5b), not aspiration:

- Arm-toggled via `ToolState::with_reduce_output(bool)` (test-only, never
  production): `grep` (path grouping, issue-338), `find` (directory compaction,
  issue-340), `bash` (output filter, ADR-0037).
- **`ls` is NOT arm-toggled**: `ls::execute` takes `_reduce` and ignores it
  (issue-339 not started). `reduce_output` true vs false yields byte-identical
  ls output, so ls has NO A/B render probe and is deliberately absent from the
  probe table. (Earlier drafts of this doc listed ls as arm-toggled; that was
  wrong.)
- Always-on, NOT covered by the arm toggle: `read` skim (issue-337), `edit`
  result classes (issue-341). These get a separate comparison axis; the doc must
  not imply `reduce_output=false` tests them.

Measured render reductions (deterministic, proxy tokens; `tool_render_probe_log`):
grep 36.4%, find 55.7%, bash 21.2% -- all with needles surviving verbatim.
find's saving is real byte reduction: past the 1000-match default limit the
listing compacts and directory grouping shows the same shown paths in fewer
bytes. bash reduces only commands its filter recognizes -- a FAILING `cargo
test` is structured-filtered (chatter collapses, the failing test + assertion
values are kept); the deterministic bash render corpus lives in adr-0037, so the
suite's bash render probe is opt-in (it compiles) and pairs with the Phase 4
live bash workload.

## Comparison axes

```
DefaultOutputReduction  reduce_output true vs false   grep/find/bash (NOT ls)
ReadSkim                skim:false vs skim:true       read
EditResultClass         shipped compact vs reference  edit
```

## Token sources (never mixed)

- Real provider usage records (`ProviderTurnCompleted.usage`) - the ONLY source
  for absolute/headline token claims. Exact, no rounding up.
- Proxy (`bench_support::est_tokens`, 4 bytes/token) - ratios only, replay and
  direct-render probes.

## Test taxonomy

### Per-tool micro probes

Two layers per tool: (1) direct render probe (reduced vs baseline output on a
fixture, assert needles survive, proxy ratio); (2) real-model probe (model must
answer an exact question from the tool output; scored mechanically).

| tool | axis | probe | quality check | behavior metrics |
|---|---|---|---|---|
| grep | reduce | many repeated-path matches; ask exact file/line/value | exact path + value in answer; needles survive | grep calls, repeat-greps, follow-up reads, turns |
| find | reduce | >1000-match generated tree (trips compaction); ask one exact path | exact target path in answer; needle survives | find calls, repeats, ls fallbacks |
| ls | (none) | NOT arm-toggled -- `_reduce` ignored (issue-339); no A/B probe | n/a | n/a |
| bash | reduce + skip-perms | tiny crate w/ failing `cargo test` (structured-filtered on failure); ask failure facts or fix+rerun | exact left/right/test, or external `cargo`/`rustc` check | bash calls, exit codes, reruns, dangerous approvals |
| read | skim | comment-heavy source w/ sentinel signatures | exact exported names/constants | read calls, full rereads after skim |
| edit | result-class | exact / tolerant / not-found / not-unique / stale | disk hash + exact outcome class | edit attempts, tolerant/not-found rate, extra turns |

### End-to-end workloads

| workload | tools | approval | primary signal |
|---|---|---|---|
| fix-failing-test-no-bash | grep/read/edit | deny gate | regression guard (existing) |
| multi-file-rename | grep/find/read/edit | deny gate | strategy change (extra edits/turns) |
| investigate-large-log | grep/read | deny gate | pure per-turn saving |
| bash-diagnose-test-failure | bash/read/grep | skip-permissions | bash filter quality |
| bash-fix-and-rerun | bash/grep/read/edit | skip-permissions | real build/test loop |
| large-tree-locate | ls/find/grep/read | deny gate | find/ls correlation |

Skip-permissions workloads still install a denying gate, call
`Agent::with_skip_permissions(true)`, assert the gate was NOT consulted, and
assert `AgentEvent::ToolAutoApprovedDangerous` fired for the expected tools -
proving which mode ran. Confinement (workspace path safety, read-before-mutate,
mutation guard) stays enforced; fixtures are per-run temp workspaces, asserted
not to be the repo, no network.

## Module layout (test-only)

```
src/bench_tokens_per_task.rs     thin #[cfg(test)] entrypoints (kept)
src/bench_tokens/
  mod.rs
  arms.rs        Arm, ComparisonAxis, ToolVariant
  config.rs      BenchConfig::from_env()
  fixtures.rs    materialize() + temp-workspace safety
  approval.rs    ZeroPromptGate + ApprovalProfile
  observer.rs    BenchObserver (extended)
  provider.rs    ScriptedProvider, selection_for_spec()
  probes.rs      TOOL_PROBES data table
  workloads.rs   WORKLOADS data table + checks
  runner.rs      replay / real / render / model-probe runners
  records.rs     RunRecord JSONL schema
  analysis.rs    Rust JSONL aggregation + verdicts
```

Adding a tool or workload is a data change (append a table row), not new control
flow. No external TOML until a second consumer needs it.

## Config surface

Every run parameter is operator-adjustable, env-first, all optional. The three
primary knobs an operator changes per run:

| knob | env | accepts | notes |
|---|---|---|---|
| mode | `IRIS_BENCH_MODE` | `deny` \| `skip-perms` (and/or arm/axis + phase selectors) | which execution/comparison mode runs; exact meaning pending operator confirmation |
| effort | `IRIS_BENCH_REASONING` | one level `off..xhigh`, OR a comma list to sweep | HELD IDENTICAL across the two arms within one A/B comparison (it is a confounder); sweeping levels is its own dimension, not an arm |
| runs (N) | `IRIS_BENCH_N` | one integer, or `anchor=10,breadth=5` per-role | repetitions per model x workload x arm cell |

Secondary knobs: `IRIS_BENCH_REAL=1`, `IRIS_BENCH_DANGEROUS_OK=1` (bash),
`IRIS_BENCH_MODELS=provider:model,...`, `IRIS_BENCH_PHASES=smoke,micro,e2e`,
`IRIS_BENCH_TOOLS`, `IRIS_BENCH_WORKLOADS`, `IRIS_BENCH_LOG`, `IRIS_BENCH_SEED`.

Invariant preserved regardless of knob values: within a single A/B comparison,
arm is the ONLY thing that differs; effort, model, reasoning, prompt, fixture,
and order are identical across the two arms. Adjusting effort/mode/N changes what
is compared, never breaks the like-for-like arm pairing.

## Metrics + connections

Per run JSONL: token cost (real usage OR proxy, tagged, never mixed), quality
(success, missing needles, exact-fact checks, file-state/exit checks), behavior
(turns, tool-call sequence, per-tool success/error/denied, repeat-call rate,
handles, bash exit codes, edit outcome classes, dangerous-approval count), and
run invariants (model, reasoning, arm, axis, fixture, approval profile).

Analysis lives in Rust (`analysis.rs`, ignored test over JSONL) - the repo is
Rust-only. Decompose the token delta into per-turn saving vs strategy change:

```
delta_input = reduced_turns * (reduced_tpt - baseline_tpt)
            + (reduced_turns - baseline_turns) * baseline_tpt
```

Connection: join per-tool micro savings to e2e workloads by tool counts;
`residual = observed_real_delta - Σ(tool_count * median_micro_saving)`.

Verdicts: reduced success < baseline -> quality regression, stop and report;
usage None -> cell invalid; small N -> descriptive only, no win claim; CI crosses
zero -> inconclusive; baseline wins -> say so. No LLM-judge scoring.

## Phasing (smallest-correct-change first)

1. (S) Merge PR #388 / ADR-0049 into the branch; verify current harness + gate
   unchanged (`ToolState::new()` still reduces by default).
2. (M) No-behavior module refactor into `src/bench_tokens/*`; replay + ignored
   real tests reproduce identical results.
3. (M) Extend observer + JSONL schema (dangerous approvals, tool sequence,
   errors, edit outcomes, exit codes).
4. (M/L) Add `ApprovalProfile::SkipPermissions` + ONE bash smoke workload;
   requires `IRIS_BENCH_REAL=1 IRIS_BENCH_DANGEROUS_OK=1`; assert audit event.
5. (L) Per-tool micro probes for grep/find/ls/bash (render + real-model).
6. (M) read/edit separate-axis probes (reported as their own axes).
7. (L) Full e2e matrix + Rust analyzer; smoke before matrix; commit sanitized
   JSONL + report under `docs/benchmarks/`.

## Risks + guardrails

| risk | mitigation |
|---|---|
| bash nondeterminism/destructiveness | temp workspace only, asserted not the repo; tiny no-network fixtures; skip requires explicit env; audit event required |
| provider variance / quota | smoke before matrix; OAuth/subscription lanes; sequential; usage-None cells invalid |
| N too small for single-digit effects | descriptive only until variance supports a claim |
| quality-scoring subjectivity | exact-answer / needle / file-state / exit-code checks only |
| prompt/order confounders | identical reasoning+config across arms; counterbalanced deterministic order; log the schedule |
| always-on read/edit overclaim | separate axis; state the reduce toggle does not test them |
| raw tool output leaking to logs | log metrics/hashes/needles, not full outputs by default |

## Reuse vs refactor

Reuse: `Arm` semantics, `with_reduce_output`, `ScriptedProvider` + proxy replay,
`BenchObserver` (extend), `ZeroPromptGate`, `materialize()`, the 3 existing
workloads + checks, `selection_for_spec`/`bench_reasoning`/`model_specs`/
`run_real_cell` wiring, `bench_support::{est_tokens, assert_min_reduction,
survives-verbatim}`. Refactor: split the single file into `bench_tokens/*`,
replace `RealRunRecord` with a serializable `RunRecord`, add `ApprovalProfile`,
turn hard-coded loops into data tables, add the Rust analyzer.
