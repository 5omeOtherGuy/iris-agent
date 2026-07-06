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

## What `input_tokens` is (metric definition -- corrected)

`input_tokens` in the logs is **gross** input, not the billed/fresh amount. The
Anthropic provider builds it as `raw_input_tokens + cache_read + cache_write`
(see `anthropic_messages.rs::merge_usage`), and `cache_read_tokens` is a **subset
already inside** `input_tokens`. In these sessions the prompt cache hit rate is
very high, so gross tokens overstate cost:

| share of gross `input_tokens` (combined defaults, median) | value |
|---|---|
| cache_read (billed 0.1x) | 93.4% |
| fresh + cache_write (billed ~1x / 1.25x) | 6.2% |
| **cost-weighted input** (fresh 1x + cache_read 0.1x; cache_write not logged) | **15.6%** |

An earlier draft of this note reported a "billed share ~52%" -- that was wrong: it
used `input_tokens / (input_tokens + cache_read)`, which double-counts cache_read
(already inside the `input_tokens` field) and is not cost-weighted. Cost figures
below use base-input-token-equivalent units: fresh 1.0x, cache_read 0.1x, output
5.0x. Cache-creation (write) tokens are **not currently logged**, so cost is a
slight **lower bound** (writes are one-time and small in high-cache sessions);
logging them is the top follow-up.

## Results

| arm | success | valid | median round-trips | median gross input | median cost-weighted input | median tool_result_bytes |
|---|---|---|---|---|---|---|
| defaults | 10/10 | 10/10 | 23 | 547,609 | 85,329 | 60,138 |
| baseline | 10/10 | 10/10 | 21 | 494,575 | 82,537 | 71,168 |

(Raw medians: defaults cost is +3.4% here, but that is round-trip-confounded --
defaults ran a median 23 round-trips vs 21. See finding 2 for the round-trip-
controlled result.)

**20/20 success, 20/20 valid.** The model reliably fixes all four bugs in order in
a single session, in both arms. Reduction shrank tool output by 15.5% (60,138 vs
71,168 B). One benign read-before-edit retry per arm; no loops.

### Two findings (cost-corrected)

**1. Chaining costs ~1.3x versus separate sessions -- not ~2x.** On gross tokens the
combined session (547,609) is 1.94x the four separate sessions summed (282,480),
but gross is ~93% cheap cache reads. On a **cost-weighted** basis the combined
session (85,329 units) is **1.27x** the four separate ones summed (66,951 units).
The premium is real -- `input_tokens` is cumulative, so a long session re-sends its
growing context every round-trip while separate sessions start fresh -- but it is
**~27%, not ~94%.**

**2. Reduction is net-favorable, and cost-weighting amplifies it.** The raw cost
mean is +3.2% for defaults (gross +9.3%), but that is round-trip-confounded
(defaults ran a median 23 round-trips vs 21; ~10.7k gross tokens each). Controlling
by round-trip count, the cost-weighted delta is **-6.5%** favoring defaults (gross
-4.2%) -- larger on cost than on gross because reduction trims the fresh, full-price
tokens, not the cheap cache reads. It is also larger than the separate suite's
cost-weighted pooled -2.9%, because the reduced output is re-sent over 18-27
round-trips and compounds.

## Verdict

- **Chaining is feasible and safe** (100% success both arms) but modestly
  **cost-expensive**: ~1.27x the four separate sessions on a cost-weighted basis
  (1.94x on gross tokens). Batching independent tasks compounds context cost, but
  the true premium is ~27%.
- **Reduction is net-favorable and its advantage grows with session length and with
  cost-weighting** (cost-weighted round-trip-controlled -6.5% here vs -2.9% pooled
  in the separate suite).
- N=10 is small and per-stratum counts are 1-2, so the -6.5% is directional. The
  robust takeaway is the **1.27x cost-weighted chaining ratio** and that gross-token
  comparisons overstate cost ~6-7x when the cache hit rate is high.
