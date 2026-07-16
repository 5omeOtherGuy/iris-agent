---
name: compaction-tuning
description: Tune Iris auto-compaction and microcompaction settings with evidence, not vibes. Use when changing trigger thresholds, keep-recent tail, microcompaction watermark or cache-timing posture, summary reserve, or hover-level defaults — or when a compaction cost/quality claim needs proof. Covers the knob inventory, the deterministic-twin-first loop, the cache-economics math, the live campaign harness, gates, and reporting conventions.
---

# Compaction Tuning

Compaction settings trade context mass against cache breaks. Every fold or
compaction apply rewrites the shared prefix: the next request re-bills the
divergent suffix at cache-WRITE prices to save cache-READ mass on all later
requests. A setting change is justified only by that arithmetic, measured —
never by "smaller context feels cheaper".

Reference material: `docs/adr/0048` (folds), `0051` (cache-aware flush),
`0054` (trigger ladder), `0055` (governor), `0044`/`0046` (carry, recall);
benchmarks in `docs/benchmarks/auto-compaction-v2-tuning.md`,
`issue-400-fold-flush-cost.md`, and `issue-372-compaction-retention.md`.

## When to use

- Changing any default in the knob inventory below.
- Adding a new fold trigger class, tier, or flush posture.
- Claiming a compaction change saves money or preserves quality.
- Running or extending a `live_harness` campaign (`hover-a`, `pilot-a`, …).

## Knob inventory

Config (`src/config.rs`, per-project overridable):

| knob | default | effect |
|---|---|---|
| `warn` / `start` / `hard` | 0.60 / 0.72 / 0.90 | trigger-ladder fractions; resolved per model against buffer-multiple floors in `TriggerLadder::resolve` (`src/wayland/trigger.rs`) |
| `keep_recent_tokens` | 8,000 (capped at window/4) | retained tail after apply |
| `hard_wait_ms` | 120,000 | max wait for background job at hard tier |
| `max_consecutive_failures` | 3 | breaker before excerpts-only degradation |
| `microcompaction` | off | ADR-0048 fold-spent-tool-results |
| `microcompaction_watermark` | 64,000 | Class-C flush backstop, independent of the compaction threshold |

Runtime seams (not config, but tuning-relevant):

- `fold_trigger` (`src/wayland/mod.rs`): flush priority — compaction
  boundary → armed breaks (selection/reasoning switch, cold resume) →
  below-min-cacheable → watermark; `tool_result_policy.cache_timing`
  selects the posture (cache-aware hold / `PressureOnly` watermark /
  `Immediate` eager).
- `cache_profile` (`min_cacheable_tokens`, `cold_after`): what "warm"
  means per provider.
- `DEFAULT_SUMMARY_RESERVE` (8,192): sizes the buffer-multiple floors and
  the `deterministic_only` cutoff (< 4× reserve).

## The loop

Never tune against live traffic first. Order:

1. **Deterministic twin.** Reproduce the setting change on the fake lane
   through the PRODUCTION seams (`Harness::maybe_auto_compact`,
   `maybe_microcompact`) — extend `src/compaction_bench.rs` or the
   gate-runnable campaign twin, not a bespoke path. Assert shape
   invariants (reduction ratios, retention needles, modeled
   prefix-divergence economics) so the change is CI-guarded forever.
2. **Live anchor.** Validate realized economics on the Anthropic OAuth
   lane — the only lane reporting cache reads AND writes (5m/1h split).
   Codex is write-blind (`write_unreported`); use it for read-side and
   large-window cells only, and never infer write mass on it.
3. **Campaign, not one-offs.** Sweeps go through `src/live_harness/`
   (`CampaignSpec`: lanes × cells × n) so rows share the schema, cells are
   resumable via the manifest, and flaky runs are excluded by the verdict
   machinery instead of hand-waving.
4. **Report.** Artifacts to `docs/benchmarks/data/<campaign>-<date>.{jsonl,md}`,
   analysis to a named `docs/benchmarks/*.md`, decision recorded in the
   relevant ADR (amend or add). Update defaults only after the report.

Run commands:

```sh
# gate-side (deterministic)
cargo test --locked compaction_bench
# live (double-gated; sequential so rows cannot interleave)
IRIS_BENCH_LIVE=1 IRIS_BENCH_CAMPAIGN=<name> \
  cargo test --locked live_campaign -- --ignored --nocapture --test-threads=1
```

## The math

Judge every aggressiveness change by amortization, per lane pricing:

```
payback_turns ≈ (divergence_mass · p_write) / (reclaimed_tokens · p_read)
sustainable   ⇔ observed inter-break gap (turns) > payback_turns
```

Compute both from `Row` fields: `cache_read`, `cache_write_5m/1h`,
`lifecycle.fold_flushes`, `lifecycle.folds_reclaimed_estimate`,
`lifecycle.compaction_generation_applied`, priced via the `PriceTable`
in `live_harness/metrics.rs`. Sweep hover bands as **(ceiling, trough)
pairs in absolute tokens** (comparable across lanes/models), not window
fractions: ceiling = trigger point, trough = post-reclamation target.
Distinguish the reclamation mechanism — one deep compact, a micro-fold
batch, or hybrid — because they reach the same trough at different cost
(a fold batch pays no summarization request but reclaims only spent tool
mass).

Mind provider price cliffs, not just cache math: Anthropic 1M-context
models bill prompts above 200k tokens at a premium tier (~2x input), so
an arm that rides a warm cache past 200k pays a structural surcharge a
≤200k hover arm never touches. Verify current pricing before relying on
the multiplier; attribute the tier per row (a request is premium iff its
prompt mass exceeds the cutoff).

Watch `estimate_error` on every row: it is a diagnostic column, but if
drift exceeds ~10% at small budgets, fix the estimator anchor before
trusting any tuning number derived from it.

## Gates — a violation fails the cell; it is never a data point

- Retention needles from covered ranges survive verbatim in the rebuilt
  context (ADR-0045 discipline).
- Spine/carry facts (task, decisions, steps) present at every post-apply
  boundary when the scenario defines them.
- Reopen rebuild is byte-equal to the final live context (live-loop G4).
- Fold stubs name their recall handle; recall succeeds when exercised.
- Hard tier always gets deterministic relief — no tuning may make relief
  depend on a live model call.
- No fabricated numbers: unavailable lane, auth failure, or missing
  window headroom is recorded as a skip or verbatim error.

## Pitfalls

- **Tuning fractions when the floor binds.** At small windows the
  buffer-multiple floors (`window − N·summary_reserve`) dominate the
  fractional thresholds; changing `start` may do nothing. Check which arm
  of `TriggerLadder::resolve` is active for the target model first.
- **Counting a fold as free.** A held fold is free (derived state); a
  *flushed* fold costs a prefix break. `eager` (`Immediate`) flushing is
  the expected-loser bound, not a default candidate.
- **Comparing across lanes without posture.** Rows are self-describing
  via `SettingsFingerprint`; never aggregate rows whose fingerprints
  differ in anything but `run_seq`.
- **Quality claims from cost runs.** Cost and quality optima differ.
  Quality claims need the paired A/B protocol (hover plan, Phase 2) with
  mechanical scoring — not eyeballing transcripts.
- **Leaking the harness into runtime.** Tuning experiments live in
  `#[cfg(test)]` trees (`compaction_bench.rs`, `live_harness/`,
  `compaction_live_bench.rs`). Runtime code never grows
  experiment-only branches; new postures must be real, tested seams.
