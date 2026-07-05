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
additionally gated on `IRIS_BENCH_REAL=1`), but a real run spends money against a
real provider (~18 multi-turn sessions for N=3 x 3 workloads x 2 arms), which is
an operator-authorized action. Until it is run and shows arm A winning with no
success regression, **the README token-efficiency claim does not ship and the
ROADMAP Milestone-2 gate is not marked satisfied** (honesty over hype, per the
issue and ADR-0036).

**Provider is not fixed to Codex.** `run_real_arm` builds the provider from the
resolved config (`build_provider(&selection, ..)`), so the cell runs whichever
provider/model the config selects. Codex (`gpt-5.5`) is only the *default*
(`ProviderId::DEFAULT`); Anthropic (`claude-sonnet-4-6`) and Antigravity are
equally wired, each with its own `record_usage`, so a Claude cell needs only
Anthropic auth + a config/model override -- no harness change. Because output
reduction interacts with a model's tokenizer and its willingness to work from
compact context, the headline SHOULD be run on at least Codex AND Claude; a
cross-provider win is a materially stronger Milestone-2 result than either alone.

Repro (costs money) -- run once per provider you want a cell for:

```
# Default provider (Codex today):
IRIS_BENCH_REAL=1 IRIS_BENCH_N=3 \
  cargo test --bin iris tokens_per_task_headline -- --ignored --nocapture

# Claude cell: select the Anthropic provider first (needs Anthropic auth), via
# either the settings file ("default_provider": "anthropic") or the
# ANTHROPIC_API_KEY env fallback; optionally pin IRIS_MODEL=claude-sonnet-4-6.
# Then run the SAME test -- the harness reads the resolved selection.
ANTHROPIC_API_KEY=... IRIS_BENCH_REAL=1 IRIS_BENCH_N=3 \
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

## Discussion

What the numbers actually say, without spin:

- **The reductions are real but small on this corpus (3.4-9.1%).** The lever
  that survives the auto-preset/zero-prompt design is grep and find *grouping*,
  and grouping only pays off in proportion to how much repeated file-path and
  structural scaffolding the raw output would have carried. The three fixtures
  are deliberately small and self-contained (so the mechanical checks stay
  unambiguous), which under-exercises the exact conditions grouping rewards --
  many hits spread across many files in shared directories. The single-digit
  result is therefore a *floor* for a favorable workload, not a ceiling for the
  feature. It is reported as measured.
- **The biggest documented levers are structurally out of this experiment.** The
  bash output filter (ADR-0037: 50-98% per class) and `read` skim (#337:
  50-72%) are the large reducers, but the auto preset defers bash approval and
  skim is opt-in, so neither can move an end-to-end, prompt-free, auto-approved
  task. This benchmark measures the reduction that a *hands-off* agent loop gets
  for free; the per-result benchmarks measure the rest. Both are true; neither
  alone is the whole story.
- **Replay is a plumbing proof, not a capability proof.** Because the tool-call
  sequence is scripted, both arms complete by construction, so "100% success in
  both arms" here means "the identical scripted fix applied and the reduced
  output still carried every needle" -- not "a model chose the right calls from
  reduced context." The needle-survival assertion closes the obvious hole (arm A
  cannot pass while having dropped an actionable fact), but it cannot close the
  reasoning hole. That is the honest boundary of the deterministic path, and it
  is why the Milestone-2 gate does not flip on replay evidence alone.
- **Two token sources, one honest split.** Replay uses a 4-bytes/token proxy
  (ratios only); the headline path uses real provider usage records (absolute
  tokens). They are never mixed in a single claim. The proxy is adequate for a
  regression fence ("did arm A stop beating arm B?"); it is not adequate for a
  headline number, which is exactly why the real path exists.

Net: the replay path is a durable, zero-cost regression guard that the reduction
plumbing keeps working and keeps preserving task-critical facts. The headline
question Milestone 2 actually asks -- fewer tokens per *completed* task, with a
real model, at no cost to success -- is not answered here and is not claimed.

## Further research / further testing

Ordered by how much each would strengthen the Milestone-2 claim:

1. **Run the real-provider headline (the actual gate), on more than one
   provider.** N >= 3 per cell, real usage records, per-cell mean
   tokens-per-completed-task + success rate + spend. This is the only path that
   proves a model completes from reduced context. Run at least **Codex
   (`gpt-5.5`) AND Claude (`claude-sonnet-4-6`)** -- the harness is
   provider-agnostic (it builds from the resolved config), and reduction
   interacts with each model's tokenizer and its tolerance for compact context,
   so a per-provider table is the honest shape and a cross-provider win is a
   materially stronger result. Blocker: operator spend authorization (and, for
   Claude, adding Anthropic auth -- only Codex/Antigravity are authed today).
   Until then the gate stays open.
2. **Grow the fixtures toward grouping's strength.** Add a search/triage workload
   with many hits across many files in shared directories (dozens of matches,
   deep trees) -- the regime where grep/find grouping compounds. This would show
   whether the end-to-end lever is genuinely single-digit or just under-sampled
   here. Keep the mechanical-check discipline (planted, unambiguous facts).
3. **Add a bash-bearing arm under a non-auto preset.** To measure the ADR-0037
   filter end-to-end (not just per-result), run a workload where the agent *is*
   allowed to run `cargo test`, under an approve-all-but-floors config, and
   assert zero *floor* violations instead of zero prompts. This widens coverage
   to the largest lever at the cost of a more complex approval story; design the
   contract before wiring it.
4. **Replace the token proxy with a tokenizer on the replay path.** The
   4-bytes/token heuristic is fine for a ratio fence but drifts from real BPE
   segmentation. A committed tokenizer (matching the target model family) would
   make the replay ratios trustworthy in absolute-ish terms and reduce the gap
   to the headline path.
5. **Track variance and turn count, not just mean tokens.** Reduced context can
   change how many turns a task takes; a token win that costs an extra turn may
   be net-neutral. The headline harness already records turns -- report the
   distribution (min/median/max) once real data exists, and flag any arm-A turn
   inflation as a finding.
6. **Guard against a success-rate regression explicitly.** The most important
   negative result would be arm A completing *fewer* real tasks than arm B
   (reduction hid something the model needed). The headline harness must treat
   any arm-A success drop as blocking; a dedicated adversarial fixture (a fact
   that only survives if a specific reduction is conservative) would stress this
   directly.
