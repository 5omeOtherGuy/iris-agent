# ADR-0037 bash output filter — token benchmark (PR 1: declarative filters)

Measured over the committed corpus of captured real command outputs in
`src/tools/bash/filter/corpus/`. Tokens estimated at 4 bytes/token; only the
ratios matter. The numbers below are asserted (as minimum bars) by the corpus
tests in `src/tools/bash/filter/corpus.rs`; regenerate the table with:

```
cargo test corpus_benchmark_report -- --nocapture
```

| class | tokens before | tokens after | reduction | filter |
|---|---|---|---|---|
| cargo test (pass) | 439 | 47 | 89% | cargo-test |
| cargo test (fail) | 255 | 183 | 28% | cargo-test |
| cargo build (compile error) | 98 | 84 | 14% | cargo-build |
| git status | 141 | 91 | 35% | git-status |
| git diff | 6482 | 6482 | 0% | (passthrough) |
| git log | 792 | 792 | 0% | (passthrough) |
| npm test (pass) | 95 | 30 | 68% | npm-test |
| npm test (fail) | 414 | 250 | 40% | npm-test |
| npm install (installer log) | 414 | 86 | 79% | npm-install |
| shellcheck (linter) | 408 | 403 | 1% | shellcheck |

## Reading the table

- **Noisy classes** (ADR-0036 bar: >= 60% with zero quality loss): cargo test
  pass 89%, npm test pass 68%, npm install 79%. Asserted by
  `corpus_noisy_classes_hit_reduction_bar`.
- **Failure classes** reduce less by design: failure detail is exempt from
  reduction. Every error message, `file:line`, and failing test name in the
  corpus survives verbatim — asserted by
  `corpus_failure_and_summary_content_survives_verbatim`.
- **git diff / git log** pass through untouched: diff hunks and commit bodies
  are signal, and safe reduction needs parsing, not line regexes. Structured
  Rust filters for cargo/git/npm are PR 2 of #336.
- **shellcheck** output is dense signal (findings + caret context); the
  declarative filter only drops blanks. Expected, not a regression.

## Overhead

`corpus_filter_overhead_under_10ms_per_call` asserts < 10 ms per call on every
corpus sample (debug build; registry compile cost excluded as a one-time
lazy-init). Observed well under 1 ms per call on the corpus.
