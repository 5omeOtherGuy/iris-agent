# Combined chained-all-four workload - Sonnet 4.6, low, N=10 per arm (2026-07-06)

One session fixes all four bugs in order (bytes -> clap -> nushell -> dayjs), each
in its own subdirectory of a single workspace. This is the same four PR-seeded
repairs as the separate suite, but chained into one conversation. Data:
[`chained-all-four-sonnet46-low-n10-2026-07-06.jsonl`](./chained-all-four-sonnet46-low-n10-2026-07-06.jsonl)
(20 rows).

## Configuration

- Model `anthropic:claude-sonnet-4-6`, reasoning `low` (identical across arms).
- Arms: `baseline` (reduction off) vs `defaults` (reduction on).
- N = 10 per arm, sharded 5 x N=2, 10 parallel processes, max round-trips 80
  (raised from 40 because one session does four repairs).
- Workload `chained-all-four-fix`: subprojects assembled by `build_chained_all_tree`
  (reuses the committed single-bug fixtures); Rust subprojects tested with
  `cargo test --manifest-path <sub>/Cargo.toml`, dayjs with `npm test --prefix dayjs`.
- Validity harness-bracketed (pristine check must fail; success = post-run check).

## Results

| arm | success | valid | median round-trips | median billed input_tok | median tool_result_bytes |
|---|---|---|---|---|---|
| defaults | 10/10 | 10/10 | 23 | 547,609 | 60,138 |
| baseline | 10/10 | 10/10 | 21 | 494,575 | 71,168 |

**20/20 success, 20/20 valid.** The model reliably fixes all four bugs in order in
a single session, in both arms. Reduction shrank tool output by 15.5% (60,138 vs
71,168 B). One benign read-before-edit retry per arm; no loops.

### Two findings

**1. Chaining costs ~2x versus separate sessions.** The combined session spends a
median 547,609 billed input tokens (defaults). The same four repairs run as four
independent sessions (separate-suite N=50 defaults medians) sum to ~282,479 tokens
-- so **one combined session is ~1.94x more expensive than four separate ones.**
`input_tokens` is cumulative: every model round-trip re-sends the whole growing
context, so by round-trip 25 the model re-bills all of bugs 1-3 while working on
bug 4. Separate sessions each start with a small, fresh context. (Cache does not
hide this: total processed is also higher for the longer arm; billed share ~52%.)

**2. Reduction helps MORE in the long chained session.** Raw median is +10.7% for
defaults, but that is round-trip-confounded (defaults happened to run a median 23
round-trips vs 21; each round-trip costs ~10,700 tokens here). Controlling by
round-trip count, the weighted delta is **-4.2%** favoring defaults -- larger than
the -1.8% pooled effect in separate sessions, because the reduced tool output is
re-sent over 18-27 round-trips instead of 6-8, so the per-round-trip saving
compounds more.

## Verdict

- **Chaining is feasible and safe** (100% success both arms) but **token-expensive**
  (~2x separate sessions) -- a session-design signal: batching independent tasks
  into one conversation compounds context cost.
- **Reduction is net-favorable and its advantage grows with session length**
  (round-trip-controlled -4.2% here vs -1.8% in the separate suite), consistent
  with the compounding mechanism.
- N=10 is small and per-stratum counts are 1-2, so the -4.2% is directional, not a
  claim. The 1.94x chaining-cost ratio is the robust takeaway.
