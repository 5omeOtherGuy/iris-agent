# Auto-compaction v2 benchmark and default tuning

Slice 9 extends the ADR-0045 production-seam benchmark and replaces the
provisional trigger defaults. Deterministic lanes use fake providers. Live
probes use the two protocol parent lanes; every model-backed summary uses
`anthropic/claude-opus-4-6` with medium thinking.

## Regenerate

```sh
cargo test --locked auto_compaction_v2_ -- --nocapture
cargo test --locked tuned_policy_reclaims_more_total_context_without_more_generations
cargo test --locked hard_and_reactive_dimensions_use_deterministic_parent_owned_apply
cargo test --locked focus_instruction_improves_needle_retention_rate

IRIS_BENCH_LIVE=1 IRIS_AUTO_COMPACTION_SESSIONS=10 \
  cargo test --locked auto_compaction_live_loop_ -- \
  --ignored --nocapture --test-threads=1
```

The live harness accepts test-only threshold and retained-tail overrides for
candidate probes: `IRIS_AUTO_COMPACTION_WARN`,
`IRIS_AUTO_COMPACTION_START`, `IRIS_AUTO_COMPACTION_HARD`, and
`IRIS_AUTO_COMPACTION_KEEP_TOKENS`. Omitting them tests production defaults.

## Worker arms

One deterministic 20 ms worker runs through the production worker/apply seam.
The foreground comparator uses the manual await path. Blocking is elapsed time
between a compaction lifecycle/apply event and the next parent request outside
hard pressure.

| arm | boundary | trigger | origin | main-loop blocked | covered reduction | needle |
|---|---|---|---|---:|---:|---:|
| background transcript | mid-turn | start | subagent | 0.0 ms | 99.7% | pass |
| background investigator | mid-turn | start | subagent | 0.0 ms | 99.7% | pass |
| foreground manual await | turn-edge | manual | subagent | 38.8 ms | 99.7% | pass |

The tests require both background arms to report `origin=subagent`; a fallback
cannot masquerade as a successful worker arm. They require less than 50 ms of
main-loop blocking and at least 75% covered-range reduction.

## Trigger and boundary dimensions

| trigger | boundary | origin | covered reduction | needle |
|---|---|---|---:|---:|
| start | mid-turn | subagent | 99.7% | pass |
| hard | turn-edge | excerpts | 99.6% | pass |
| reactive | reactive resend | excerpts | 98.9% | pass |

The hard and reactive arms assert deterministic, parent-owned apply. The
reactive arm injects one typed overflow and succeeds on the bounded resend.

## Focus retention

The planted fact appears only in older covered history. Five deterministic
trials use the generic instruction; five name the fact in `/compact <focus>`'s
worker instruction.

| arm | retained | trials | rate |
|---|---:|---:|---:|
| control | 0 | 5 | 0% |
| focused | 5 | 5 | 100% |

The contract asserts a strict improvement and 100% retention in the focused
arm. This measures steering by the focus instruction, not general summary
quality.

## Long-horizon policy sweep

Both candidates run through trigger v2 with 30 seeded message pairs, a complete
`recall` tool-call/result group, and 60 growth turns. `POLICY-NEEDLE-7f3a9` and
`RECALL-LOOP-HIT-22b7` must remain in rebuilt context. Every generation must
retain a recall marker.

| policy | generations | average total reduction | shallowest total reduction | maximum post/start | fact | recall markers |
|---|---:|---:|---:|---:|---:|---:|
| provisional 0.55/0.65/0.85, keep 20k | 6 | 48.5% | 41.2% | 12,214/21,299 (57.3%) | pass | 6/6 |
| selected 0.60/0.72/0.90, keep 8k | 4 | 58.3% | 54.6% | 10,693/23,592 (45.3%) | pass | 4/4 |

The selected policy must compact at least three generations, use no more
generations than the provisional policy, improve average total-context
reduction, preserve both needles, and retain one recall marker per generation.

## Why the earlier live compactions looked minimal

`context_tokens_after_apply` measures message history, not system prompt or tool
schema overhead. For one apply:

```text
reclaimed = covered_original - summary
true_before = context_after_apply + reclaimed
covered reduction = reclaimed / covered_original
total reduction = reclaimed / true_before
```

Under the provisional policy, one Haiku probe's shallowest apply was
19,820 -> 18,319: 1,501 tokens reclaimed. The worker reduced its covered slice
by 83.4%, but that slice was small, so the whole message context fell only 7.6%.
The 20,000-token retained-tail target protected the newest pair-closed tool
groups while the 0.65 start threshold launched compaction early. The summary
was already small; range selection was the limiting factor.

Test-only Haiku candidate probes isolated the policy change:

| policy | compactions | shallowest before -> after | reclaimed | covered reduction | total reduction | worst G1 | gates |
|---|---:|---:|---:|---:|---:|---:|---|
| provisional, keep 20k | 3 | 19,820 -> 18,319 | 1,501 | 83.4% | 7.6% | below 200 ms | pass |
| 0.60/0.72/0.90, keep 6k | 2 | 28,464 -> 13,809 | 14,655 | 96.1% | 51.5% | 1.5 ms | pass |
| 0.60/0.72/0.90, keep 8k | 2 | 30,428 -> 15,487 | 14,941 | 97.4% | 49.1% | 1.7 ms | pass |

The 8k tail keeps 1,678 more recent tokens than 6k for 2.4 percentage points
less total reduction. It is the selected default. These are one-session tuning
probes, not the final 10-session protocol.

## Cache economics

Every live-loop entry reports worker usage, so the summarization-request cache
hit is `workerUsage.cacheReadInputTokens / inputTokens`. The parent provider is
also recorded around each apply:

- Anthropic reports cache writes. The row compares reported write tokens on the
  request before apply with the first request after apply.
- Codex reports cache reads but no writes. Its row is labeled
  `derived-fresh-input` and uses `inputTokens - cacheReadInputTokens` on both
  sides. It is not presented as a provider-reported write.

The first post-retuning Haiku full run observed nine worker rows at 0.000 and
one usage-blind row. Across 23 paired applies, the parent reported 45,012 cache
write tokens before and 442,524 after compaction, a 9.831× amplification. This
is the measured cost of rewriting the cached prefix, not a reclamation ratio.
The final two full protocol runs supply the closing table in
`auto-compaction-live-loop.md`. Earlier live evidence remains valid: the
slice-7 Haiku loop observed three sessions near 0.999 worker cache hit and seven
at 0.000; the committed Anthropic write-side probe measured 1,758/1,761 input
tokens written after compaction. Codex write accounting remains unavailable by
provider contract.

After quota recovery, one Codex instrument smoke forced two compactions and
reported a 0.999 worker cache-hit rate. Parent derived fresh input increased
from 5,249 to 22,158 tokens across paired applies (4.221×). The row is labeled
derived because Codex still does not report writes.

## Provider-native arm

The live native arm is capability-gated, so failed capability is reported
instead of replaced by portable fallback numbers.

| lane | backend observation | portable text | production native arm |
|---|---|---|---|
| `anthropic/claude-haiku-4-5` | `400 invalid_request_error` | no | unavailable; default off |
| `openai-codex/gpt-5.4-mini` | one v2 opaque block | no | rejected by durable-text invariant |

The portable worker remains the measured default on both lanes.

## Decision

Set defaults to warn/start/hard `0.60/0.72/0.90` and retain 8,000 recent
tokens. The change delays job launch, gives the pair-safe covered prefix time to
grow, reclaims roughly half the total message context in the live candidate,
and reduces deterministic long-horizon generation count without weakening the
retention contracts.
