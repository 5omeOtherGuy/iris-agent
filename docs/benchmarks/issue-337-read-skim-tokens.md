# read skim mode — token benchmark (issue #337)

Measured over the committed corpus of real source files in
`src/tools/skim/corpus/` (openai/codex, earendil-works/pi-mono, CPython —
copied verbatim). Tokens estimated at 4 bytes/token; only the ratios matter.
Both columns are the *rendered* `read` output (line numbers included),
produced through the production seam — `read::execute` with and without
`skim: true` — so the guards (never-worse, emptied-non-empty, data-format
passthrough) sit inside the measurement. Debug build, page cache warmed,
overhead asserted best-of-three under 10 ms per call.

The comment-heavy rows are asserted as minimum bars (>= 50%) by
`src/tools/skim/corpus.rs`, built on `src/tools/bench_support.rs` (recipe:
the `token-efficiency-benchmark` skill in `.pi/skills/`). Regenerate with:

```
cargo test skim_benchmark_report -- --nocapture
```

| class | tokens before | tokens after | reduction | via |
|---|---|---|---|---|
| rust (comment-heavy) | 2419 | 1168 | 52% | skim |
| typescript (comment-heavy) | 4777 | 1345 | 72% | skim |
| python (docstring-heavy) | 1033 | 483 | 53% | skim |
| typescript (comment-light) | 1319 | 1291 | 2% | skim |
| json (data format) | 854 | 854 | 0% | full (file type is never skimmed) |

Reading guide:

- The three comment-heavy rows are the classes skim exists for; their >= 50%
  bars are asserted, the exact figures drift with fixture updates.
- The comment-light row is an honesty sample: skim applies but saves little.
  No bar is asserted; the never-worse guard is what keeps such reads from
  ever costing more than a full read.
- The JSON row proves data formats (JSON/YAML/TOML/XML/CSV and unknown
  extensions) pass through byte-identical to a full read.
- Quality loss is asserted, not eyeballed: every rendered skim line must be
  byte-identical to the original file line it is numbered as, and every
  signature line (`fn`/`struct`/`export`/`def`/…) in each raw fixture must
  survive verbatim. Skim reads never satisfy read-before-edit; a full read
  is required before `edit`/`write`.
