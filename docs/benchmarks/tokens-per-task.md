# Tokens per completed task -- end-to-end benchmark (issue #210, Milestone 2)

Does Iris's default-on tool-output reduction lower the prompt tokens spent to
COMPLETE a realistic task, without lowering task success? Plan:
[`docs/BENCHMARK_PLAN.md`](../BENCHMARK_PLAN.md). Running log of decisions and
deviations: [`tokens-per-task-notebook.md`](./tokens-per-task-notebook.md).

Two arms, identical except one benchmark-only switch:

- **Arm A -- defaults:** bash filter (ADR-0037), grep grouping (#338), find
  grouping (#340) active (shipped behavior).
- **Arm B -- baseline:** those default-on reductions disabled. Memory/context
  rails (byte/line caps ADR-0008, handle offload ADR-0011) stay on in both arms,
  so arm B is "shaping off," not "unbounded."

The switch is a measurement affordance, not a user feature: a per-run
`ToolState.reduce_output` flag threaded to the tool render seams. The baseline
arm is reachable ONLY via the test-only `ToolState::with_reduce_output(false)`,
so it cannot leak into a real session (deliberately NOT an env var or process
global -- both were flagged as leak/race risks in review). A normal
`ToolState::new()` is always `true` (reductions active); proven unchanged by
`output_reductions_default_active_and_switch_is_explicit` plus the whole
existing tool/corpus suite. See `BENCHMARK_PLAN.md` for the "env var preferred"
deviation rationale (the headline runs in-process, so no env var is needed).

## What this benchmark measures -- and what it does not

Both arms run under the ADR-0032 **auto preset** with the safety floors active
and a zero-prompt gate, identical across arms. Under auto, `bash` never
auto-approves (its sandbox-preflight path is deferred), so every workload is
designed so the AGENT uses only auto-approvable tools (read/grep/find + clean
in-workspace edit) and the HARNESS runs the mechanical success check. The
end-to-end reduction lever is therefore **grep grouping and find grouping** --
the "large search results / large test logs" acceptance signal.

Out of scope end-to-end (stated plainly, per ADR-0036 and the issue):

- **bash output filtering (ADR-0037)** -- auto-bash is deferred, so a bash-using
  workload cannot run prompt-free under auto. Its proof stays per-result
  ([`adr-0037-bash-filter-tokens.md`](./adr-0037-bash-filter-tokens.md)).
- **`read` skim (#337), grep `maxPerFile` (#338)** -- opt-in, identical across
  arms.

## Replay path (deterministic, CI, no cost)

A fixed successful tool-call script per workload is replayed by a fake provider;
the real built-in tools run over committed fixtures, so tool OUTPUTS are real and
differ by arm. Prompt tokens are an estimated proxy (`bench_support::est_tokens`,
4 bytes/token) over the transcript the provider is sent each turn -- **a ratio,
not an exact token count**. The three fixtures and their mechanical checks:

| workload | fixture | agent tools | success check (harness) |
|---|---|---|---|
| fix-failing-test | 4-file Rust lib, off-by-one in `parse_len` | grep, read, edit | `rustc --test` exits 0 |
| multi-file search-and-edit | 5-file tree using `MAX_RETRIES` | grep, read, edit | no `MAX_RETRIES` left; `MAX_ATTEMPTS` present |
| investigate-large-log | 4 captured cargo-test log shards, one planted fact | grep, read | answer carries planted `8192`/`8191` |

Result (`cargo test --bin iris tokens_per_task_replay_report -- --nocapture`):

| workload | turns | arm B proxy tokens | arm A proxy tokens | reduction | both succeed | prompts |
|---|---|---|---|---|---|---|
| fix-failing-test | 4 | 2417 | 2336 | 3.4% | yes | 0 |
| multi-file search-and-edit | 12 | 14730 | 13531 | 8.1% | yes | 0 |
| investigate-large-log | 3 | 1296 | 1178 | 9.1% | yes | 0 |

Asserted contracts (`bench_tokens_per_task::replay`, run in the gate):

- both arms pass the mechanical success check (identical fix/answer applied);
- arm A proxy tokens < arm B by a margin (>= 32 est tokens);
- zero approval prompts in either arm (the injected gate fails if consulted).

Reading the numbers honestly:

- The reductions are **single-digit percent** (3.4-9.1%). This is a grep/find
  *grouping* lever on small fixtures (the #338 grep-grouping lever is 3-27%; find
  grouping favors many files in shared directories, which these fixtures
  under-exercise). No rounding up: these are the measured figures.
- **What replay proves:** the token plumbing (smaller tool output => smaller
  transcript => fewer input tokens) and the success mechanics, deterministically
  and per-release.
- **What replay does NOT prove:** because the tool-call sequence is scripted,
  both arms succeed by construction. Replay does not show that a real model still
  COMPLETES the task when it must reason from the reduced context -- the hard
  half of the Milestone-2 acceptance signal. That needs the real-provider run
  below.

## Headline path (real provider, N >= 3) -- PENDING operator run

Status: **not yet run.** The harness is wired
(`bench_tokens_per_task::replay::tokens_per_task_headline`, `#[ignore]`d and
additionally gated on `IRIS_BENCH_REAL=1`), but a real run spends money against
the OpenAI Codex provider (~18 multi-turn sessions for N=3 x 3 workloads x 2
arms), which is an operator-authorized action. Until it is run and shows arm A
winning with no success regression, **the README token-efficiency claim does not
ship and the ROADMAP Milestone-2 gate is not marked satisfied** (honesty over
hype, per the issue and ADR-0036).

Repro (costs money):

```
IRIS_BENCH_REAL=1 IRIS_BENCH_N=3 \
  cargo test --bin iris tokens_per_task_headline -- --ignored --nocapture
```

It prints per-cell rows (workload x arm x run) with REAL usage-record input
tokens (`AgentEvent::ProviderTurnCompleted.usage.input_tokens`), turn count,
success, and the zero-prompt flag. The operator records the mean
tokens-per-completed-task per cell, the success rate (must be 100% in arm A; any
regression is a blocking finding), and the actual spend here.

Risk to watch on the real run: under auto the model must not call `bash`
(`cargo test`) or the zero-prompt gate denies it and the run is invalid. The
workload prompts instruct read/grep/find/edit only; if a model still reaches for
bash, redesign the prompt -- never the floor.

## Measurement conditions

Debug build. Replay tokens are `bench_support::est_tokens` (4 bytes/token);
only ratios are meaningful. Fixtures are committed under
`src/bench_fixtures/tokens_per_task/`; the log shards are built from the real
captured `src/tools/bash/filter/corpus/cargo-test-fail.txt` content with one
planted fact. The scripts are the contract; this doc is a snapshot.

Repro (replay, no cost):

```
cargo test --bin iris bench_tokens_per_task
cargo test --bin iris tokens_per_task_replay_report -- --nocapture
```
