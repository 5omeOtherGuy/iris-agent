# Chained PR-seeded suite -- Sonnet 4.6, low, N=50 per workload/arm

High-N sharded run of the expanded chained repair suite. Each workload is seeded
from a real PR pattern, requires a failing test before the final passing test,
and runs under `--dangerously-skip-permissions` in an isolated temp workspace.

Run shape:

- Model: `anthropic:claude-sonnet-4-6`.
- Reasoning: iris `low` (Sonnet 4.6 manual thinking budget 4096).
- Workloads: 6 chained repair workloads.
- N: 50 per workload per arm -> 600 sessions.
- Parallelism: 5 shards per arm, N=10 per shard, 10 processes total.
- Safety: every shard wrote a unique log file (`IRIS_BENCH_LOG`); logs were merged
  after all processes exited. `IRIS_BENCH_SHARD` + `IRIS_BENCH_RUN_OFFSET` are
  included in each row for debugging.

## Result

- 600/600 valid rows, 0 invalid/noncompliant rows, 0 process-level errors.
- 590/600 task success. The only failures were `chained-ampi-private-docs-fix`,
  split evenly: A 45/50 and B 45/50. These runs got `npm test` passing but failed
  the mechanical anti-test-weakening check because the source did not contain the
  required `docs/private/` exclusion.
- No success regression: defaults matched baseline success on every workload.
- No massive loop signal: median turns matched in every workload; max tool calls
  stayed in the 7-9 range. Total tool errors were low (14/600 rows' calls), with
  A=9 and B=5; most were edit-before-read or old-string mismatch recoveries.
- Token verdict remains honest: overall BASELINE WINS because two cells were
  baseline-cheaper on median (`private-docs`, `fold-resume`). One cell is a clear
  supported win (`openai-summary`, -8.6% median, Welch CI clears zero). The rest
  are small/noisy/inconclusive.

## Practical read

The benchmark harness shape works: high-N sharding is safe with separate log
files, all rows remain analyzable after merge, and the safety section surfaces
success/tool-loop regressions separately from token deltas. The product claim is
still not blanket-supported: compaction helps strongly when the reduced output is
material (`openai-summary`) but is neutral or loses on several small-output cells.

# Tokens-per-task analysis

Cells: 600 valid, 0 invalid (usage None / missing fields), 0 errored, 0 lines skipped.

OVERALL VERDICT: BASELINE WINS (no claim)

## Paired A (defaults) vs B (baseline) -- real usage tokens

| model | workload | N a/b | success a/b | med in a/b | turns a/b | delta | mechanism | eff / turns | result-bytes delta | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| anthropic:claude-sonnet-4-6 | chained-ampi-github-token-fix | 50/50 | 100%/100% | 52498/52527 | 6/6 | -28 (-0.1%) | per-turn (same turn count) | -28 / +0 | +0 | INCONCLUSIVE (small N or overlapping spread) |
| anthropic:claude-sonnet-4-6 | chained-ampi-pack-untracked-fix | 50/50 | 100%/100% | 56966/57014 | 7/7 | -47 (-0.1%) | per-turn (same turn count) | -48 / +0 | +0 | INCONCLUSIVE (small N or overlapping spread) |
| anthropic:claude-sonnet-4-6 | chained-ampi-private-docs-fix | 50/50 | 90%/90% | 52003/51957 | 6/6 | +46 (+0.1%) | per-turn (same turn count) | +46 / +0 | +537 | BASELINE WINS (no claim) |
| anthropic:claude-sonnet-4-6 | chained-iris-fold-resume-fix | 50/50 | 100%/100% | 50791/49110 | 6/6 | +1680 (+3.4%) | per-turn (same turn count) | +1681 / +0 | +970 | BASELINE WINS (no claim) |
| anthropic:claude-sonnet-4-6 | chained-iris-recall-span-fix | 50/50 | 100%/100% | 52885/53330 | 6/6 | -444 (-0.8%) | per-turn (same turn count) | -445 / +0 | -930 | INCONCLUSIVE (small N or overlapping spread) |
| anthropic:claude-sonnet-4-6 | chained-openai-summary-fix | 50/50 | 100%/100% | 98080/107262 | 7/7 | -9182 (-8.6%) | per-turn (same turn count) | -9182 / +0 | -3068 | SUPPORTED (descriptive; still needs N) |

`delta` is A - B median input tokens (negative = defaults cheaper). `mechanism` says where it came from: `per-turn` (same turn count -- a genuine reduction effect) or `fewer/more turns` (dominated by whole eliminated/added turns of mostly-fixed prompt overhead, a STRATEGY difference confounded with the reduction). `eff / turns` is the arithmetic split, but because per-turn tokens are cumulative it is a clean reduction signal ONLY when turn counts match. `result-bytes delta` is real tool-output bytes in context (A - B); ~0 means the reduction never fired for that cell's tool path.

## Safety / loop signals

| model | workload | success a/b | turns a/b | tool calls med a/b | tool calls max a/b | tool errors a/b |
|---|---|---|---|---|---|---|
| anthropic:claude-sonnet-4-6 | chained-ampi-github-token-fix | 100%/100% | 6/6 | 7.0/7.0 | 7/7 | 0/0 |
| anthropic:claude-sonnet-4-6 | chained-ampi-pack-untracked-fix | 100%/100% | 7/7 | 7.0/7.0 | 8/9 | 0/0 |
| anthropic:claude-sonnet-4-6 | chained-ampi-private-docs-fix | 90%/90% | 6/6 | 7.0/7.0 | 9/8 | 3/5 |
| anthropic:claude-sonnet-4-6 | chained-iris-fold-resume-fix | 100%/100% | 6/6 | 6.0/7.0 | 7/8 | 0/0 |
| anthropic:claude-sonnet-4-6 | chained-iris-recall-span-fix | 100%/100% | 6/6 | 6.0/6.0 | 8/7 | 2/0 |
| anthropic:claude-sonnet-4-6 | chained-openai-summary-fix | 100%/100% | 7/7 | 8.0/7.5 | 9/9 | 3/1 |

This section is the N-run compaction-safety check: if defaults keep the same success rate without higher turns, higher tool-call maxima, or a tool-error spike, the reduced output did not make the task harder to interpret or trigger tool loops for this workload.

## Significance (Welch 95% CI on mean input-token saving, B - A; + = defaults cheaper)

| model | workload | mean saving | 95% CI | clears zero |
|---|---|---|---|---|
| anthropic:claude-sonnet-4-6 | chained-ampi-github-token-fix | +299 | [-63, +661] | no |
| anthropic:claude-sonnet-4-6 | chained-ampi-pack-untracked-fix | +1007 | [-1258, +3272] | no |
| anthropic:claude-sonnet-4-6 | chained-ampi-private-docs-fix | +648 | [-2385, +3681] | no |
| anthropic:claude-sonnet-4-6 | chained-iris-fold-resume-fix | -2578 | [-4407, -749] | no |
| anthropic:claude-sonnet-4-6 | chained-iris-recall-span-fix | -2 | [-4862, +4857] | no |
| anthropic:claude-sonnet-4-6 | chained-openai-summary-fix | +8980 | [+3601, +14360] | yes |

A cell is SUPPORTED only when its saving CI clears zero, success held, and N is adequate -- a real, statistically defensible reduction. A CI that crosses zero stays INCONCLUSIVE no matter how large N is.
