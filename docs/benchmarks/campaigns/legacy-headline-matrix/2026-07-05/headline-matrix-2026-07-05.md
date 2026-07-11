# Headline tokens-per-task matrix -- 2026-07-05

Authorized real-provider run. Configuration:

- Models: `openai-codex:gpt-5.4-mini`, `openai-codex:gpt-5.3-codex-spark`,
  `anthropic:claude-haiku-4-5` (the default 3-spec matrix; all OAuth lanes).
- Reasoning effort: `low` (held identical across BOTH arms -- a confounder).
- Workloads: `fix-failing-test`, `multi-file-search-and-edit`,
  `investigate-large-log` (deny-gate, non-bash; per-run temp workspaces).
- Arms: B (baseline, reductions off) then A (defaults, reductions on).
- N = 5 runs per cell. 90 real sessions total. Raw log:
  `headline-matrix-2026-07-05.jsonl` (schema v3, one line per run).

Tokens are REAL provider usage records (`input_tokens`), never byte proxies.
Analysis rendered by the deterministic `analysis.rs` (`tokens_per_task_report`).

## Verdict: BASELINE WINS -- no tokens-per-task claim shipped

- **No success regression** anywhere (the stop-and-report trigger): every cell
  held task success, and `gpt-5.4-mini / investigate-large-log` improved it
  (A 100% vs B 80%). Two cells tied at 80%/80% (haiku multi-file).
- **No consistent token win.** 3 of 9 cells had defaults cheaper, 6 had baseline
  cheaper. Overall verdict is BASELINE WINS.
- **Turn-count variance dominates** (confirmed again at N=5). Every large swing
  (+/-20% to +66%) is flagged `more/fewer turns (confounded w/ strategy)`: the
  arm changed how many provider turns it took, and each turn is mostly fixed
  system-prompt + tool-schema overhead. That is strategy noise, not a reduction.
- **On the 4 clean same-turn-count cells** (`per-turn` mechanism) the reduction's
  net effect is within noise: -4.1%, +0.1%, +0.5%, +2.2%. The one negative
  (gpt-5.4-mini multi-file, -4.1%, result-bytes -913) is INCONCLUSIVE by spread.

The per-tool render probes still show large raw-output reductions (grep 36%,
find 56%, read 83%), but that per-tool shrink does NOT translate into a
measurable per-completed-task token reduction at this scale: the reduced bytes
are a small fraction of each turn's cumulative context, and turn-count variance
swamps them. Honest conclusion: the Milestone-2 tokens-per-completed-task claim
is NOT supported by this run. The ROADMAP gate stays open; no README claim.

# Tokens-per-task analysis

Cells: 90 valid, 0 invalid (usage None / missing fields), 0 errored, 0 lines skipped.

OVERALL VERDICT: BASELINE WINS (no claim)

## Paired A (defaults) vs B (baseline) -- real usage tokens

| model | workload | N a/b | success a/b | med in a/b | turns a/b | delta | mechanism | eff / turns | result-bytes delta | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| anthropic:claude-haiku-4-5 | fix-failing-test | 5/5 | 100%/100% | 29327/37258 | 4/5 | -7931 (-21.3%) | fewer turns (confounded w/ strategy) | -479 / -7452 | -29 | INCONCLUSIVE (small N or overlapping spread) |
| anthropic:claude-haiku-4-5 | investigate-large-log | 5/5 | 100%/100% | 27993/21220 | 4/3 | +6773 (+31.9%) | more turns (confounded w/ strategy) | -300 / +7073 | +1350 | BASELINE WINS (no claim) |
| anthropic:claude-haiku-4-5 | multi-file-search-and-edit | 5/5 | 80%/80% | 92277/67720 | 8/7 | +24557 (+36.3%) | more turns (confounded w/ strategy) | +14883 / +9674 | +64 | BASELINE WINS (no claim) |
| openai-codex:gpt-5.3-codex-spark | fix-failing-test | 5/5 | 100%/100% | 33768/33049 | 6/6 | +719 (+2.2%) | per-turn (same turn count) | +719 / +0 | +563 | BASELINE WINS (no claim) |
| openai-codex:gpt-5.3-codex-spark | investigate-large-log | 5/5 | 100%/100% | 27675/16621 | 5/3 | +11054 (+66.5%) | more turns (confounded w/ strategy) | -27 / +11081 | -831 | BASELINE WINS (no claim) |
| openai-codex:gpt-5.3-codex-spark | multi-file-search-and-edit | 5/5 | 100%/100% | 68982/88418 | 8/11 | -19436 (-22.0%) | fewer turns (confounded w/ strategy) | +4678 / -24114 | -1134 | INCONCLUSIVE (small N or overlapping spread) |
| openai-codex:gpt-5.4-mini | fix-failing-test | 5/5 | 100%/100% | 23554/23519 | 4/4 | +35 (+0.1%) | per-turn (same turn count) | +35 / +0 | -118 | BASELINE WINS (no claim) |
| openai-codex:gpt-5.4-mini | investigate-large-log | 5/5 | 100%/80% | 21452/21353 | 4/4 | +99 (+0.5%) | per-turn (same turn count) | +99 / +0 | +455 | BASELINE WINS (no claim) |
| openai-codex:gpt-5.4-mini | multi-file-search-and-edit | 5/5 | 100%/100% | 35486/37014 | 5/5 | -1528 (-4.1%) | per-turn (same turn count) | -1528 / +0 | -913 | INCONCLUSIVE (small N or overlapping spread) |

`delta` is A - B median input tokens (negative = defaults cheaper). `mechanism` says where it came from: `per-turn` (same turn count -- a genuine reduction effect) or `fewer/more turns` (dominated by whole eliminated/added turns of mostly-fixed prompt overhead, a STRATEGY difference confounded with the reduction). `eff / turns` is the arithmetic split, but because per-turn tokens are cumulative it is a clean reduction signal ONLY when turn counts match. `result-bytes delta` is real tool-output bytes in context (A - B); ~0 means the reduction never fired for that cell's tool path.
