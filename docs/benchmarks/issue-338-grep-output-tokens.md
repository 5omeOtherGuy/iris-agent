# grep content-mode output — token benchmark (issue #338)

Measured over the committed corpus of real source files in
`src/tools/grep_corpus/` (this repo's tool modules and the generated codemap
index, copied verbatim). Tokens estimated at 4 bytes/token; only the ratios
matter. Every number is produced through the production seam (`grep()` in
content mode), so grouping, long-line clamping, and the per-file cap all sit
inside the measurement. Debug build, page cache warmed, overhead asserted
best-of-three under 10 ms per call.

Contracts are asserted by `src/tools/grep_corpus/corpus.rs`, built on
`src/tools/bench_support.rs` (recipe: the `token-efficiency-benchmark` skill in
`.pi/skills/`). Regenerate with:

```
cargo test grep_benchmark_report -- --nocapture
```

## Grouping vs. the ungrouped baseline

"before" is the raw ungrouped form (`path:line:content` per rendered line,
`render_flat`); "after" is the grouped production output (path printed once per
file, `> line│` markers). Grouping is asserted **parity-or-better** on every
class by `corpus_grouping_is_parity_or_better_vs_flat`.

| class | tokens before | tokens after | reduction | via |
|---|---|---|---|---|
| high-match (one file) | 1985 | 1447 | 27% | group |
| many-files | 4833 | 4522 | 6% | group |
| long-lines | 8120 | 7898 | 3% | group |

Grouping never inflated output on any real fixture (smallest margin +3%), so
the **"grouped only if smaller" guard is not shipped** — it would never fire.
The per-file `path:` prefix that grouping removes always outweighs the
per-group header and `│` markers it adds. The guard is a no-op on real grep
results; adding it would be dead code. Numbers above are the evidence.

## Per-file cap effect

"before" is the uncapped grouped output; "after" caps content-mode matches per
file (`maxPerFile`), summarizing the rest with a `… N more matches in this
file` count line. A file with fewer matches than the cap is untouched (the
many-files row: no file has more than the cap of 20 `fn ` matches).

| class | tokens before | tokens after | reduction | via |
|---|---|---|---|---|
| high-match (one file) | 1447 | 180 | 88% | cap=5 |
| many-files | 4522 | 4522 | 0% | cap=20 |
| long-lines | 7898 | 2217 | 72% | cap=20 |

No silent drops: `corpus_per_file_cap_shrinks_and_accounts_for_every_match`
asserts `shown matches + summed omitted counts == exact total` on every class,
and the header total plus every matched file path always survive
(`corpus_reports_exact_total_and_every_file_path`).

The cap is opt-in (default unlimited) to preserve the codebase's no-arbitrary-
clamp parity: existing behavior for under-cap results is byte-identical, and
the two "not clamped" tests stay green unmodified. See the PR body for the
default-on recommendation.

## Context-line default audit

Context is the dominant token spend. Grouped tokens for the high-match class at
each context width:

| context | est tokens | vs. default |
|---|---|---|
| 0 | 759 | -48% |
| 1 | 1255 | -13% |
| 2 (default) | 1447 | — |
| 3 | 1587 | +10% |

At the default (2), context lines are ~48% of the class's grouped tokens
(1447 vs 759 with none). Dropping to context=1 saves ~13% while still showing
one line either side of each match. This is a readability-vs-tokens tradeoff,
not a silent change: see the PR body for the proposal. Default is left at 2 in
this change.

## Measurement conditions

Debug build, page cache warmed, best-of-three timing.
`corpus_overhead_under_10ms_per_call` asserts < 10 ms per call on every sample.
Tokens are 4-byte estimates; quote ratios, never absolute counts.
