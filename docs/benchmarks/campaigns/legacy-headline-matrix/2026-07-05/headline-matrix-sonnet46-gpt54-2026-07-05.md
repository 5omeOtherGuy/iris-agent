# Headline tokens-per-task matrix (stronger models) -- 2026-07-05

Second authorized real-provider run, operator-requested: the two stronger models
at low thinking, to see whether a per-turn reduction signal is cleaner than on
the default 3-spec set (BASELINE WINS, mixed sign -- see
`headline-matrix-2026-07-05.md`).

- Models: `anthropic:claude-sonnet-4-6`, `openai-codex:gpt-5.4` (full, not
  `-mini`; both served on OAuth, smoke-confirmed reachable).
- Reasoning effort: `low` (held identical across BOTH arms).
- Workloads: `fix-failing-test`, `multi-file-search-and-edit`,
  `investigate-large-log`. Arms: B (baseline) then A (defaults). N = 5.
- 60 real sessions. Raw log:
  `headline-matrix-sonnet46-gpt54-2026-07-05.jsonl` (schema v3).

Tokens are REAL provider usage records; analysis by the deterministic
`analysis.rs`.

## Verdict: BASELINE WINS (no claim) -- but directionally favorable, INCONCLUSIVE

Honest, non-cherry-picked reading:

- **No success regression, no tie.** 100% task success in BOTH arms of ALL 6
  cells (verified by direct count).
- **Sign is now consistent.** 5 of 6 cells have defaults cheaper
  (-1.5%, -2.5%, -36.9%, -5.0%, -1.0%); only 1 is baseline-cheaper. Contrast the
  3-spec run (mixed 3/9 vs 6/9).
- **All 4 clean same-turn-count cells favor defaults** (`per-turn` mechanism):
  -1.5%, -2.5%, -5.0%, -1.0%, each with a matching negative `result-bytes` delta
  (-291, -1168, -1295, -539) -- the reduction fired and each turn was modestly
  cheaper. But every one is INCONCLUSIVE: at N=5 the small delta does not clear
  the run-to-run spread.
- **The overall BASELINE WINS is dragged by one confounded cell:** gpt-5.4
  `investigate-large-log`, where A took 4 turns vs B's 2 (`more turns`,
  +94.7%). That is strategy variance (result-bytes delta +23, essentially nil),
  not the reduction losing.

**Interpretation.** With stronger models the per-turn reduction shows a
consistent favorable sign (unlike the noisier default set), suggesting a small
real effect (~1-5% per clean cell). But N=5 cannot statistically separate it
from turn-to-turn variance, and one turn-count-confounded cell is enough to make
the conservative overall verdict BASELINE WINS. The claim remains UNSUPPORTED:
no README claim, ROADMAP gate stays open. The path to significance is more runs
on the clean same-turn-count cells (a larger N on single-strategy tasks), not a
wider model sweep.

Note (analyzer, not acted on here to avoid tuning-to-result): a turn-count
`confounded` cell with A cheaper resolves INCONCLUSIVE, but with A pricier
resolves BASELINE WINS -- an asymmetry. Both mean "no claim", so the shipping
decision is unchanged; flagged as a candidate refinement to raise separately.

# Tokens-per-task analysis

Cells: 60 valid, 0 invalid (usage None / missing fields), 0 errored, 0 lines skipped.

OVERALL VERDICT: BASELINE WINS (no claim)

## Paired A (defaults) vs B (baseline) -- real usage tokens

| model | workload | N a/b | success a/b | med in a/b | turns a/b | delta | mechanism | eff / turns | result-bytes delta | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| anthropic:claude-sonnet-4-6 | fix-failing-test | 5/5 | 100%/100% | 29245/29685 | 4/4 | -440 (-1.5%) | per-turn (same turn count) | -440 / +0 | -291 | INCONCLUSIVE (small N or overlapping spread) |
| anthropic:claude-sonnet-4-6 | investigate-large-log | 5/5 | 100%/100% | 22496/23074 | 3/3 | -578 (-2.5%) | per-turn (same turn count) | -578 / +0 | -1168 | INCONCLUSIVE (small N or overlapping spread) |
| anthropic:claude-sonnet-4-6 | multi-file-search-and-edit | 5/5 | 100%/100% | 53454/84656 | 6/9 | -31202 (-36.9%) | fewer turns (confounded w/ strategy) | -2983 / -28219 | -102 | INCONCLUSIVE (small N or overlapping spread) |
| openai-codex:gpt-5.4 | fix-failing-test | 5/5 | 100%/100% | 22295/23469 | 4/4 | -1174 (-5.0%) | per-turn (same turn count) | -1174 / +0 | -1295 | INCONCLUSIVE (small N or overlapping spread) |
| openai-codex:gpt-5.4 | investigate-large-log | 5/5 | 100%/100% | 21950/11272 | 4/2 | +10678 (+94.7%) | more turns (confounded w/ strategy) | -594 / +11272 | +23 | BASELINE WINS (no claim) |
| openai-codex:gpt-5.4 | multi-file-search-and-edit | 5/5 | 100%/100% | 34191/34547 | 5/5 | -356 (-1.0%) | per-turn (same turn count) | -356 / +0 | -539 | INCONCLUSIVE (small N or overlapping spread) |

`delta` is A - B median input tokens (negative = defaults cheaper). `mechanism` says where it came from: `per-turn` (same turn count -- a genuine reduction effect) or `fewer/more turns` (dominated by whole eliminated/added turns of mostly-fixed prompt overhead, a STRATEGY difference confounded with the reduction). `eff / turns` is the arithmetic split, but because per-turn tokens are cumulative it is a clean reduction signal ONLY when turn counts match. `result-bytes delta` is real tool-output bytes in context (A - B); ~0 means the reduction never fired for that cell's tool path.
