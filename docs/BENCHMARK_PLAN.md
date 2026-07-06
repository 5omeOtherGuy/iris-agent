# Benchmark Plan: tokens-per-completed-task (Milestone 2, issue #210)

Written before the run. Deviations from this plan are recorded honestly in the
report (`docs/benchmarks/tokens-per-task.md`) and the lab notebook
(`docs/benchmarks/tokens-per-task-notebook.md`).

## Question

Do Iris's default-on tool-output reductions lower the prompt tokens spent to
complete a realistic coding task, without lowering task success? This is the
Milestone-2 gate (ROADMAP): "a benchmark shows that handle-returning tool
outputs reduce prompt tokens without reducing task success on at least one
realistic workflow such as large search results, large test logs, or multi-file
inspection."

Honesty rule (ADR-0036, issue #210): the claim cites the committed report; if
the baseline ties or beats defaults anywhere, that is the finding and the
marketing claim does not ship. A task-success regression in the defaults arm is
stop-and-investigate, not a footnote.

## Arms

Two arms, identical in every respect except one benchmark-only switch.

- **Arm A - defaults.** Iris as shipped: bash output filtering (ADR-0037), grep
  grouping (#338), and find grouping/compaction (#340) all active.
- **Arm B - baseline.** The same runtime with those default-on reductions
  disabled, so tool output is the pre-reduction (flat / raw) form. The
  memory/context rails stay on in both arms (byte/line caps ADR-0008, handle
  offload ADR-0011) so arm B is "naive shaping off," not "unbounded."

### The arm switch (measurement affordance, not a user feature)

A single benchmark-only, per-run flag selects the arm: `ToolState.reduce_output`,
threaded to the tool render seams (grep/find/bash). `true` (the only value a
normal `ToolState::new()` ever produces) = reductions active = shipped behavior;
`false` = the baseline arm.

The baseline arm is reachable ONLY through the test-only
`ToolState::with_reduce_output(false)`, which the harness calls to build each
arm explicitly and independently. Deliberately NOT an environment variable or
process global: an env-driven switch read in production would let an ambient
variable silently change a normal session's tool output (flagged in review), and
a process global would race across parallel tests (flagged by the oracle). The
in-process flag avoids both -- it is structurally incapable of leaking into a
real run.

> Deviation from the pinned plan ("env var preferred"): the real-provider
> headline runs IN-PROCESS (it constructs the provider directly, not via a
> standalone `iris -p` process), so no env var is needed to reach arm B, and
> adding one only introduces the leak/race risks above. The requirement it must
> satisfy -- an arm-LEVEL toggle, not per-call `raw:true` -- is met by the
> per-run `ToolState` flag.

The default (`ToolState::new().reduce_output == true`) is proven unchanged by a
test, and the entire existing tool/corpus suite stays green. Scope of the
switch: default-on reductions only. It does NOT touch opt-in behaviors (grep
`maxPerFile`, `read` skim) or the safety rails (caps, handles), because those
are identical across arms by construction.

## Workloads

Three committed fixtures, each with a mechanically checkable success criterion
the harness evaluates automatically (no eyeballing). Every workload is designed
so the AGENT uses only auto-approvable tools (read/grep/find/ls + clean
in-workspace edit/write); the mechanical success check runs in the HARNESS,
outside the agent turn. See "Approvals" for why.

1. **fix-failing-test** (multi-file inspection). Fixture: a small Rust crate
   with one failing unit test caused by a bug in a source file. Agent: locate
   the bug with grep/read, fix it with `edit`. Success (harness): `cargo test`
   on the fixture is green.
2. **multi-file search-and-edit** (large search results). Fixture: a tree with
   a symbol/string that must be changed at every occurrence across many files.
   Agent: locate all sites with grep/find, `edit` each. Success (harness): the
   resulting files match the committed expected snapshot (expected diff
   applied), and no stray occurrences remain.
3. **investigate-large-log** (large test logs). Fixture: a committed captured
   test/build log with a single planted fact (e.g. the one failing test's
   assertion detail) buried in noise. Agent: triage with grep/read, answer.
   Success (harness): the final answer contains the planted fact.

Fixtures are copied to a scratch dir per run; the committed fixture is never
mutated. The reduction lever per workload is the search/inspection tool output
(grep/find), which is exactly the "large search results / large test logs"
acceptance signal.

## Metrics (per arm x workload)

- **Prompt tokens.** Real provider usage records
  (`AgentEvent::ProviderTurnCompleted.usage.input_tokens`) summed across the
  run, for the real-provider headline. For the deterministic replay path, an
  estimated proxy over the messages actually sent to the provider (never
  reported as exact tokens).
- **Tokens-per-completed-task.** Prompt tokens divided by completed tasks
  (0 tokens-per-task is undefined for a failed task; a failed task is reported
  as a failure, never as a cheap success).
- **Task success rate.** Fraction of runs whose mechanical check passed.
- **Turn count.** Provider round trips.
- **Wall cost.** Real-provider spend, reported in the PR body.

## Runs

- **Replay / regression path (CI, no cost).** Fake-provider harness with a fixed
  successful tool-call script per workload. Runs both arms, asserts (a) the
  mechanical success check passes, (b) arm A prompt-token proxy < arm B by a
  margin, (c) zero approval prompts. Deterministic; runs per release in the gate.
- **Headline path (real provider, opt-in).** N >= 3 real-provider runs per cell
  (workload x arm). Prompt tokens from usage records. Opt-in behind an env gate
  so CI and normal `cargo test` never spend money. Repro commands and actual
  spend are recorded in the report and PR body.

## Approvals

Both arms run under the ADR-0032 **auto preset**, identical across arms; no
bypass is added and the non-bypassable safety floors stay active. Under auto v1,
`bash` never auto-approves (its safe-auto path needs a sandbox preflight that is
deferred), so every workload is designed so the AGENT never calls `bash`: the
agent inspects and edits with auto-approvable tools, and the harness runs the
`cargo test` / diff / grep success checks. The harness asserts zero interactive
approval prompts per run; any prompt observed invalidates the run. The replay
path injects a recording approval gate (as `nexus_tests` does) that fails if
consulted, identical across arms.

`--approve` / skip-permissions is NOT used for the headline: it would measure a
bypass mode, not the auto preset. It may back a separate, clearly-labeled,
non-headline exploratory run only.

## Explicitly out of scope of the end-to-end measurement

- **Bash output filtering (ADR-0037) end-to-end.** Auto-bash is deferred, so a
  bash-using workload cannot run prompt-free under auto. The bash filter's proof
  stays at the per-result corpus level (`docs/benchmarks/adr-0037-bash-filter-tokens.md`).
  The arm switch still disables it (arm-B completeness + a unit test), but no
  workload exercises it.
- **`read` skim (#337)** and **grep `maxPerFile` (#338)**: opt-in, model-chosen,
  identical across arms.
- New efficiency features, tool-behavior changes, cost dashboards (#206),
  compaction changes.

## Success criteria (definition of done)

1. Gate green; default-behavior tests prove the switch changes nothing when
   unset.
2. This plan committed before the run.
3. All 3 workloads have committed fixtures + mechanical checks the harness
   evaluates automatically.
4. Replay path proven in CI-compatible time: both arms, success asserted, arm A
   proxy tokens < arm B on the recorded sessions, zero prompts.
5. Headline table: >= 3 real-provider runs per cell, per-cell mean
   tokens-per-completed-task, success rate 100% in arm A (any regression is a
   blocking finding, not merged over). If real cells are not run, the report
   says so plainly and the README claim does not ship.
6. ROADMAP Milestone-2 gate row cites the report; README updated only if arm A
   wins with no success regression.
7. PR open referencing #210 with the table, spend, and repro commands. Not
   merged.

## Repro commands

Filled in the report once wired. Sketch:

```
# Replay / regression (CI, no cost):
cargo test --bin iris bench_tokens_per_task
cargo test --bin iris tokens_per_task_replay_report -- --nocapture

# Real-provider headline (opt-in, costs money):
IRIS_BENCH_REAL=1 IRIS_BENCH_N=3 \
  cargo test --bin iris tokens_per_task_headline -- --ignored --nocapture
```
