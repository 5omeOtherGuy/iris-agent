# Chained real-PR suite - Sonnet 4.6, low, N=50 per workload/arm (2026-07-06)

Real-provider run of the reseeded chained repair suite (four workloads
reconstructed from merged OSS PRs). Headline tokens come from provider usage
records (`input_tokens`, uncached/billed). Companion data:
[`chained-suite-sonnet46-low-n50-2026-07-06.jsonl`](./chained-suite-sonnet46-low-n50-2026-07-06.jsonl)
(400 rows).

## Configuration

- Model: `anthropic:claude-sonnet-4-6`, reasoning effort `low` (held identical across arms).
- Arms: `baseline` (reduction off) vs `defaults` (reduction on). Reduction is the only difference.
- N = 50 per workload/arm, sharded 5 x N=10, 10 parallel processes (one per shard x arm), max tool round-trips 40.
- Workloads: `chained-clap-conflict-panic-fix` (clap #5298), `chained-bytes-sign-extend-fix`
  (bytes #732), `chained-nushell-not-precedence-fix` (nushell #11672),
  `chained-dayjs-tz-locale-fix` (dayjs #2420).
- Validity: harness-bracketed - the fixture's own check must FAIL pristine, and success is the
  post-run mechanical check (model-independent; see `fixture_starts_broken`).

## Safety / non-regression (the first thing that must hold)

| workload | arm | n | success | valid | median turns | median calls |
|---|---|---|---|---|---|---|
| clap | defaults | 50 | 50/50 | 50/50 | 8 | 7 |
| clap | baseline | 50 | 50/50 | 50/50 | 8 | 7 |
| bytes | defaults | 50 | 50/50 | 50/50 | 7 | 7 |
| bytes | baseline | 50 | 50/50 | 50/50 | 7 | 7 |
| nushell | defaults | 50 | 50/50 | 50/50 | 6 | 5 |
| nushell | baseline | 50 | 50/50 | 50/50 | 6 | 5 |
| dayjs | defaults | 50 | 50/50 | 50/50 | 6 | 6 |
| dayjs | baseline | 50 | 50/50 | 50/50 | 6 | 6 |

**400/400 success, 400/400 valid, identical median turns/calls per workload across arms.**
Reduction never reduced task success and never changed the amount of work the model did. Tool
errors were the benign "read before edit" retry only (no loops); notably slightly more frequent
in baseline for clap (25 vs 18) and nushell (16 vs 13).

## Tokens: raw vs round-trip-controlled

`input_tokens` scales strongly with the number of model round-trips (each re-sends the cumulative
context), and round-trip count is largely model nondeterminism. Raw medians are therefore
confounded by round-trip composition; the honest comparison stratifies by round-trip count.

| workload | tool output (defaults vs base) | RAW mean delta | round-trip-controlled delta | read |
|---|---|---|---|---|
| clap | -9.5% | -2.8% (t=-0.47, ns) | **-5.8%** | real win |
| bytes | -5.3% | +0.8% (t=0.69, ns) | -0.0% | wash |
| nushell | +2.0% | -1.6% (t=-0.43, ns) | -2.1% (one-stratum) | weak/noise |
| dayjs | +19.7% | +11.5% (t=2.28, sig) | +1.4% | wash (raw is round-trip artifact) |

**Pooled round-trip-controlled delta (all 4 workloads): -1,308 tok = -1.8%** of the overall
baseline mean (73,855 tok). Small favorable tilt, concentrated almost entirely in clap.

> **Metric note (cost basis).** The deltas above are on GROSS `input_tokens` (fresh +
> cache_read + cache_write; see notebook Entry 31). Prompt-cache hit rate is high here, so
> ~85% of gross is cache_read billed at 0.1x -- gross deltas overstate cost. Cost-weighted
> (fresh 1x + cache_read 0.1x; cache_write not logged, so a lower bound), the effect is
> LARGER, not smaller, because reduction trims fresh full-price tokens: pooled round-trip-
> controlled = **-2.9%** (vs -1.8% gross); per-workload cost-weighted mean A vs B = bytes
> -0.4%, clap -6.6%, nushell -1.1%, dayjs +7.6%. Conclusions are unchanged (clap wins, rest
> wash), but any cost claim should use the cost-weighted figures.

### clap is the one clear, significant effect

Consistent across strata and significant at the modal round-trip count:

| round-trips | defaults mean | baseline mean | delta |
|---|---|---|---|
| 6 | 55,615 (n14) | 55,276 (n12) | +0.6% |
| 7 | 62,747 (n5) | 67,530 (n9) | -7.1% |
| 8 | 76,061 (n13) | 88,377 (n11) | **-13.9%** (Welch t=-3.17) |
| 9 | 90,663 (n9) | 94,219 (n8) | -3.8% |
| 10 | 98,316 (n4) | 101,243 (n3) | -2.9% |

clap is where reduction both shrinks tool output (-9.5%) AND the reduced content is re-sent over
many round-trips (median 8, up to 14) - so the per-round-trip saving compounds. bytes shrinks
output too (-5.3%) but runs a fixed 7 round-trips over a smaller reducible share, so the token
total does not move. dayjs's raw +11.5% ("significant") is a round-trip-composition artifact:
defaults happened to make slightly more tool calls (6.6 vs 6.3 mean) and land more high-round-trip
runs; controlled, it is a +1.4% wash on a workload whose 97-line source gives reduction almost
nothing to trim.

## Verdict

- **Success and safety: identical and perfect (100% both arms).** No regression from reduction.
- **Tokens: modest, workload-dependent, favorable.** Pooled round-trip-controlled -1.8%, driven by
  clap (-5.8%, significant); bytes/nushell/dayjs are washes at matched round-trips. No workload is
  meaningfully worse with reduction once round-trips are controlled.
- **Mechanism (refined):** the win appears where tool output is both reduced and re-sent across many
  round-trips. It is not a blanket per-task saving; it is real but concentrated.
- **Not a headline efficiency claim.** One clearly-winning workload out of four, a -1.8% pooled
  effect, and heavy round-trip confounding do not support a global "fewer tokens per task" claim.
  They do support: reduction is safe (no success/turn regression) and net-favorable, most on
  large-output, many-round-trip repairs. Consistent with the prior N=50 suite (Entry 27): size-gated
  and round-trip-confounded, report output-conditioned rather than blanket.
