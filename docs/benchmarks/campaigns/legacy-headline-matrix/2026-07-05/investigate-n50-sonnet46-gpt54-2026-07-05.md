# High-power investigate safety run -- Sonnet 4.6 + GPT-5.4, low, N=50

Operator-requested high-N run for `investigate-large-log`. The purpose of this
run is **compaction safety**, not a large product-savings claim: does reduced
output preserve task success and avoid making the model loop, ask for more tools,
or produce wrong answers? A tiny whole-run token delta is expected here because
the reduced grep payload is only a small slice of the full prompt/tool-schema
transcript.

Effort checked first: Sonnet 4.6 is a MANUAL-BUDGET model, so iris `low` sends
`thinking.budget_tokens = 4096` (NOT Anthropic's named `low` effort -- that scale
is adaptive-tier only, e.g. Sonnet 5 / Opus 4.7+, where iris `minimal` maps to
Anthropic `low`). GPT-5.4 `low` -> `reasoning.effort = "low"` (direct). Effort
held identical across both arms.

- Models: `anthropic:claude-sonnet-4-6`, `openai-codex:gpt-5.4`.
- Workload: `investigate-large-log` only (via `IRIS_BENCH_WORKLOAD`).
- Arms B then A, reasoning `low`, N = 50 per arm -> 200 real sessions.
- Raw log: `investigate-n50-sonnet46-gpt54-2026-07-05.jsonl` (schema v3).

## Result: safety/non-regression evidence, not a headline savings claim

- **No success regression:** both models were 100%/100% across defaults and
  baseline.
- **No loop signal:** Sonnet stayed at 3/3 turns, 2.0/2.0 median tool calls,
  2/2 max tool calls, 0/0 tool errors. GPT-5.4 stayed at 3/3 median turns,
  3.0/3.0 median tool calls, 6/6 max tool calls, 0/0 tool errors.
- **Token delta is secondary:** Sonnet showed a small statistically detectable
  end-to-end input-token reduction (~2.4%); GPT-5.4 was directionally cheaper
  but within noise. That is expected and not the main point of this run: the
  tool output being compacted was too small a share of the whole transcript to
  support a large overall savings claim.

## Follow-up workload implication

To test product-level savings, the benchmark needs a chained real-world repair
where reduced tool outputs are a large fraction of the run: discovery (`find`),
multiple broad greps, reads, edits, then a noisy `cargo test`/`npm test` bash
loop. That workload was added separately as `chained-openai-summary-fix`, seeded
from the real PR #404 bug (OpenAI reasoning summary request missing
`summary: "auto"`).

# Tokens-per-task analysis

Cells: 200 valid, 0 invalid (usage None / missing fields), 0 errored, 0 lines skipped.

OVERALL VERDICT: INCONCLUSIVE (small N or overlapping spread)

## Paired A (defaults) vs B (baseline) -- real usage tokens

| model | workload | N a/b | success a/b | med in a/b | turns a/b | delta | mechanism | eff / turns | result-bytes delta | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| anthropic:claude-sonnet-4-6 | investigate-large-log | 50/50 | 100%/100% | 22499/23074 | 3/3 | -575 (-2.5%) | per-turn (same turn count) | -575 / +0 | -1168 | SUPPORTED (descriptive; still needs N) |
| openai-codex:gpt-5.4 | investigate-large-log | 50/50 | 100%/100% | 16381/16579 | 3/3 | -199 (-1.2%) | per-turn (same turn count) | -198 / +0 | -655 | INCONCLUSIVE (small N or overlapping spread) |

`delta` is A - B median input tokens (negative = defaults cheaper). `mechanism` says where it came from: `per-turn` (same turn count -- a genuine reduction effect) or `fewer/more turns` (dominated by whole eliminated/added turns of mostly-fixed prompt overhead, a STRATEGY difference confounded with the reduction). `eff / turns` is the arithmetic split, but because per-turn tokens are cumulative it is a clean reduction signal ONLY when turn counts match. `result-bytes delta` is real tool-output bytes in context (A - B); ~0 means the reduction never fired for that cell's tool path.

## Safety / loop signals

| model | workload | success a/b | turns a/b | tool calls med a/b | tool calls max a/b | tool errors a/b |
|---|---|---|---|---|---|---|
| anthropic:claude-sonnet-4-6 | investigate-large-log | 100%/100% | 3/3 | 2.0/2.0 | 2/2 | 0/0 |
| openai-codex:gpt-5.4 | investigate-large-log | 100%/100% | 3/3 | 3.0/3.0 | 6/6 | 0/0 |

This section is the N-run compaction-safety check: if defaults keep the same success rate without higher turns, higher tool-call maxima, or a tool-error spike, the reduced output did not make the task harder to interpret or trigger tool loops for this workload.

## Significance (Welch 95% CI on mean input-token saving, B - A; + = defaults cheaper)

| model | workload | mean saving | 95% CI | clears zero |
|---|---|---|---|---|
| anthropic:claude-sonnet-4-6 | investigate-large-log | +541 | [+397, +685] | yes |
| openai-codex:gpt-5.4 | investigate-large-log | +319 | [-1607, +2246] | no |

A cell is SUPPORTED only when its saving CI clears zero, success held, and N is adequate -- a real, statistically defensible reduction. A CI that crosses zero stays INCONCLUSIVE no matter how large N is.
