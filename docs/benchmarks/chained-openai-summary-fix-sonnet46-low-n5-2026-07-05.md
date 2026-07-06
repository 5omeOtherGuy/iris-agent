# Chained OpenAI summary fix -- Sonnet 4.6, low, N=5 per arm

Small parallel live smoke for the PR-404-seeded chained repair workload after two
fixes to the workload design:

- Prompt now explicitly says to run plain `cargo test` first, before reading or
  editing any file.
- Runs that do not include a failing bash exit before the final passing bash exit
  are marked invalid/noncompliant and excluded from token comparison.

The run used two simultaneous processes with separate logs:
`IRIS_BENCH_ARM=baseline` and `IRIS_BENCH_ARM=defaults`, Sonnet 4.6, low, N=5 per
arm, workload `chained-openai-summary-fix`.

## Result

- 10/10 rows were valid and successful.
- Every row reproduced a failing test before the final passing test (`101` before
  final `0`).
- Safety: same median turns (7/7) and same median tool calls (7.0/7.0). Defaults
  had lower max calls (8 vs 9) but had one edit error; baseline had zero tool
  errors. No approval prompts.
- Tokens: defaults were directionally cheaper at the median (-4.2%) but N=5 is
  noisy; Welch CI crosses zero, so no claim.

This is a useful design check: the workload now actually exercises the intended
reproduce-failure -> inspect/search/read -> edit -> verify chain, while the
analysis surfaces safety signals separately from token deltas.

# Tokens-per-task analysis

Cells: 10 valid, 0 invalid (usage None / missing fields), 0 errored, 0 lines skipped.

OVERALL VERDICT: INCONCLUSIVE (small N or overlapping spread)

## Paired A (defaults) vs B (baseline) -- real usage tokens

| model | workload | N a/b | success a/b | med in a/b | turns a/b | delta | mechanism | eff / turns | result-bytes delta | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| anthropic:claude-sonnet-4-6 | chained-openai-summary-fix | 5/5 | 100%/100% | 97690/101987 | 7/7 | -4297 (-4.2%) | per-turn (same turn count) | -4297 / +0 | -377 | INCONCLUSIVE (small N or overlapping spread) |

`delta` is A - B median input tokens (negative = defaults cheaper). `mechanism` says where it came from: `per-turn` (same turn count -- a genuine reduction effect) or `fewer/more turns` (dominated by whole eliminated/added turns of mostly-fixed prompt overhead, a STRATEGY difference confounded with the reduction). `eff / turns` is the arithmetic split, but because per-turn tokens are cumulative it is a clean reduction signal ONLY when turn counts match. `result-bytes delta` is real tool-output bytes in context (A - B); ~0 means the reduction never fired for that cell's tool path.

## Safety / loop signals

| model | workload | success a/b | turns a/b | tool calls med a/b | tool calls max a/b | tool errors a/b |
|---|---|---|---|---|---|---|
| anthropic:claude-sonnet-4-6 | chained-openai-summary-fix | 100%/100% | 7/7 | 7.0/7.0 | 8/9 | 1/0 |

This section is the N-run compaction-safety check: if defaults keep the same success rate without higher turns, higher tool-call maxima, or a tool-error spike, the reduced output did not make the task harder to interpret or trigger tool loops for this workload.

## Significance (Welch 95% CI on mean input-token saving, B - A; + = defaults cheaper)

| model | workload | mean saving | 95% CI | clears zero |
|---|---|---|---|---|
| anthropic:claude-sonnet-4-6 | chained-openai-summary-fix | +6735 | [-5442, +18912] | no |

A cell is SUPPORTED only when its saving CI clears zero, success held, and N is adequate -- a real, statistically defensible reduction. A CI that crosses zero stays INCONCLUSIVE no matter how large N is.
