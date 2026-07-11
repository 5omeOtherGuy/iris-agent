# Pilot-A findings

First live campaign of the `live_harness` module (PR #559). Anthropic-only, low
effort, n=2, cells S1 + S3 + S4-small at compaction defaults. Artifacts:
`docs/benchmarks/campaigns/pilot-a/2026-07-10/run3/pilot-a-2026-07-10.{jsonl,md,manifest}`
(the three archived runs sit under `docs/benchmarks/campaigns/pilot-a/2026-07-10/run{1,2,3}/`).

The campaign validated plumbing, schema, and artifact writing, and surfaced two
findings. Both are fixed in-tree with in-gate deterministic tests; the live path
stays double-gated (`#[ignore]` + `IRIS_BENCH_LIVE=1`).

## Finding 1 -- S1 could not exercise compaction (fixed)

**Symptom.** S1 (aggressive-fill) reached `tier=hard` at 34k context, yet
`compaction_generation_applied` stayed null in every row. S1 never compacted.

**Root cause.** S1 seeded the whole runaway context in the pre-turn transcript,
then drove ONE provider round-trip (a single "summarize" turn). Auto-compaction
governs only at pair-closed boundaries BETWEEN round-trips within a continuing
turn (`compaction_governor::govern`; post-turn it early-returns on
`!turn_continues`). A one-round-trip turn has no mid-turn boundary, so the
governor never ran. The scenario silently under-drove and still reported Pass.

**Fix.** S1 now loads the seed BELOW the start tier and drives several
sequential tool-call round-trips inside ONE turn (mirrors
`auto_compaction_live_loop`'s real-tool loop): the model reads one file per
reply, so each read closes a pair boundary mid-turn. Seed + scripted read
results cross start then hard before the final round-trip, leaving a continuing
hard-tier boundary where #552 current-turn coverage fires.

S1 now carries its own success criteria (`Scenario::verify_run`): a run must
observe >= 3 boundaries AND >= 1 compaction lifecycle event, else it is a Fail
with reason `S1 produced no compaction`. The runner marks such a run
`HardFailure` (failing the verdict) and records the reason in the `.md` report's
`## Scenario failures` block -- a scenario that under-drives is now loud, not a
silent green.

Tests (in-gate, no provider):
- `scenario::tests::s1_seed_loads_below_start_and_scripted_reads_cross_hard_within_planned_round_trips`
  -- pure token arithmetic against the estimator.
- `scenario::tests::s1_verify_run_fails_without_compaction_and_passes_with_one`.
- `runner::tests::s1_drives_multiple_boundaries_and_compacts_mid_turn` --
  real Harness/governor + scripted provider; asserts a mid-turn apply across
  >= 3 boundaries.
- `campaign::tests::report_surfaces_scenario_failures_verbatim`.

## Finding 2 -- estimate_error was a per-turn broadcast, not a per-request delta (fixed; case b)

**Symptom.** S4-small opening requests showed a consistent ~-3,650-token
`estimate_error` (run0 req0 -3649, req3 -3663; run1 req0 -3695, req3 -3644),
while other rows only trailed by the just-generated output tokens.

**Root cause -- harness sampling artifact, NOT a runtime undercount.** The
column was sampled wrongly on two counts:
1. It was not the estimator at all. It was `context_diagnostics().measured`
   (`measure_context`, provider-anchored), so it could never serve its
   documented purpose (catching estimator drift).
2. It was sampled ONCE per turn (at turn end) and broadcast to every request in
   that turn. Reproduced from the jsonl: each turn's `context_estimate_tokens`
   equals that turn's LAST request `input + output`, copied onto the earlier
   requests. The opening request of a two-round-trip turn (small opening input)
   was diffed against the turn-end total, i.e. the ~3.6k Cargo.toml read mass
   added later in the same turn -- a like-for-unlike delta.

Trigger timing is unaffected: the runtime triggers on `measure_context`, which
anchors on provider-reported totals once a usage lands; the pure estimator's
fixed framing/schema blind spot never makes tiers fire late in steady state. So
this is case (b) -- fix the harness column, not the runtime estimator.

**Fix.** `RecordingProvider` now captures the pure estimator
(`message_token_estimate` summed) over each request's EXACT payload at request
time (`CapturedUsage::estimate_tokens`). The row's `context_estimate_tokens` is
that per-request value and `context_measured_tokens` is that request's provider
input, so `estimate_error = measured - estimate` is a genuine per-request
like-for-like delta. The per-turn `context_diagnostics()` sampling is removed.
The honest residual (estimator trails provider by the uncounted
system-prompt/tool-schema/framing mass) now shows as a small consistent drift
instead of a phantom -3.6k.

Test (in-gate, no provider):
- `runner::tests::estimate_error_is_per_request_not_a_turn_end_broadcast`.

## Run 2 — 2026-07-10 (post-#566)

Verdict FAIL by the new fail-loud rule, working as intended: S1 run 0 drove five
round-trips, crossed start, and never compacted (`S1 produced no compaction`);
run 1 compacted at boundary 4 (gen 1, subagent). Root cause: live
provider-anchored context ran ~15% below the estimator arithmetic the shape
test used, so four reads topped out between start and hard (28,056 vs hard
29,491) and compaction depended on the start-tier background race instead of
the hard-tier deterministic backstop. Fix: six round-trips with a 20%
provider-discount margin assertion in the shape test.

Run 2 also produced the first live cache break-even point (S1 run 1, boundary
4): the apply broke the warm prefix (cache reads 23.4k -> 2.2k, 9.7k re-write
at 1.25x) in exchange for input dropping 25.7k -> 11.9k -- payback in roughly
one request. Artifacts (archived under
`docs/benchmarks/campaigns/pilot-a/2026-07-10/`): `run3/pilot-a-2026-07-10.{jsonl,md}`
(this run) and `run1/pilot-a-2026-07-10-run1.{jsonl,md}` (the prior run).
