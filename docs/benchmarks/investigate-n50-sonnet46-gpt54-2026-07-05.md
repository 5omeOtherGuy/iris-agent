# High-power investigate cell -- Sonnet 4.6 + GPT-5.4, low, N=50

Operator-requested high-N run to give the cleanest, cheapest cell
(`investigate-large-log`) enough power to move past INCONCLUSIVE. Effort checked
first: Sonnet 4.6 is a MANUAL-BUDGET model, so iris `low` sends
`thinking.budget_tokens = 4096` (NOT Anthropic's named `low` effort -- that scale
is adaptive-tier only, e.g. Sonnet 5 / Opus 4.7+, where iris `minimal` maps to
Anthropic `low`). GPT-5.4 `low` -> `reasoning.effort = "low"` (direct). Effort
held identical across both arms.

- Models: `anthropic:claude-sonnet-4-6`, `openai-codex:gpt-5.4`.
- Workload: `investigate-large-log` only (via `IRIS_BENCH_WORKLOAD`).
- Arms B then A, reasoning `low`, N = 50 per arm -> 200 real sessions.
- Raw log: `investigate-n50-sonnet46-gpt54-2026-07-05.jsonl` (schema v3).

## Result: FIRST statistically significant win -- on Sonnet 4.6, narrowly scoped

- **Sonnet 4.6: SUPPORTED.** Defaults save a median 575 / mean 541 input tokens
  (-2.4%) at the SAME turn count (3/3), 100% success in both arms. The reduction
  visibly fired (result-bytes -1168). Welch 95% CI on the mean saving
  [+397, +685] clears zero; corroborated out-of-band by a permutation test
  (p < 0.0001) and a bootstrap median CI [+572, +578], with all 50/50 A-runs
  cheaper than the B median. Low variance (sd ~330-400) is what makes the small
  effect visible.
- **GPT-5.4: INCONCLUSIVE.** Same direction (-1.2% median, mean saving +319) but
  the Welch CI [-1607, +2246] spans zero (permutation p = 0.75; only 29/50 A-runs
  beat the B median). GPT-5.4's per-run variance on this task is enormous
  (sd ~4400-5300, 2-4 turns unpredictably), which swamps the reduction.
- **Overall verdict: INCONCLUSIVE** -- one cell supported, one not; no blanket
  claim across both models.

## Analyzer change this run forced

N=50 exposed a flaw: the verdict test used a range-overlap guard
(`max_A >= min_B`) that can NEVER certify at large N (one outlier A-run always
exceeds the cheapest B-run), so it wrongly called the p<0.0001 Sonnet cell
INCONCLUSIVE. Replaced it with a standard Welch 95% CI on the mean saving: a cell
is SUPPORTED only when that CI clears zero (plus success held + adequate N). It
correctly certifies Sonnet and correctly leaves the noisy GPT-5.4 cell
inconclusive, so it is a real statistical test, not a rubber stamp. Locked in by
two new gate cases (overlapping-ranges-but-separated-means -> Supported;
large-N-high-variance -> Inconclusive).

## Honest scope -- what this does and does NOT support

This is the strongest evidence in the study, but it is ONE model on ONE workload
-- the most reduction-favorable one (a log grep where compaction directly shrinks
the answer-bearing output). It supports a narrow statement: *on a stable model
(Sonnet 4.6) doing grep-heavy log triage, output reduction gives a small (~2.4%)
but statistically defensible token saving at equal success.* It does NOT support
a blanket "reduces tokens per task" claim: GPT-5.4 is within noise here, and the
prior 3-workload matrices were mixed/turn-count-dominated. No README claim and no
ROADMAP-gate closure on the strength of one cell; this is logged as measured
evidence for review.

# Tokens-per-task analysis

Cells: 200 valid, 0 invalid (usage None / missing fields), 0 errored, 0 lines skipped.

OVERALL VERDICT: INCONCLUSIVE (small N or overlapping spread)

## Paired A (defaults) vs B (baseline) -- real usage tokens

| model | workload | N a/b | success a/b | med in a/b | turns a/b | delta | mechanism | eff / turns | result-bytes delta | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| anthropic:claude-sonnet-4-6 | investigate-large-log | 50/50 | 100%/100% | 22499/23074 | 3/3 | -575 (-2.5%) | per-turn (same turn count) | -575 / +0 | -1168 | SUPPORTED (descriptive; still needs N) |
| openai-codex:gpt-5.4 | investigate-large-log | 50/50 | 100%/100% | 16381/16579 | 3/3 | -199 (-1.2%) | per-turn (same turn count) | -198 / +0 | -655 | INCONCLUSIVE (small N or overlapping spread) |

`delta` is A - B median input tokens (negative = defaults cheaper). `mechanism` says where it came from: `per-turn` (same turn count -- a genuine reduction effect) or `fewer/more turns` (dominated by whole eliminated/added turns of mostly-fixed prompt overhead, a STRATEGY difference confounded with the reduction). `eff / turns` is the arithmetic split, but because per-turn tokens are cumulative it is a clean reduction signal ONLY when turn counts match. `result-bytes delta` is real tool-output bytes in context (A - B); ~0 means the reduction never fired for that cell's tool path.

## Significance (Welch 95% CI on mean input-token saving, B - A; + = defaults cheaper)

| model | workload | mean saving | 95% CI | clears zero |
|---|---|---|---|---|
| anthropic:claude-sonnet-4-6 | investigate-large-log | +541 | [+397, +685] | yes |
| openai-codex:gpt-5.4 | investigate-large-log | +319 | [-1607, +2246] | no |

A cell is SUPPORTED only when its saving CI clears zero, success held, and N is adequate -- a real, statistically defensible reduction. A CI that crosses zero stays INCONCLUSIVE no matter how large N is.
