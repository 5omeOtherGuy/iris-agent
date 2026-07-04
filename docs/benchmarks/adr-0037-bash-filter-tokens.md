# ADR-0037 bash output filter — token benchmark

Measured over the committed corpus of captured real command outputs in
`src/tools/bash/filter/corpus/`. Tokens estimated at 4 bytes/token; only the
ratios matter. The numbers below are asserted (as minimum bars) by the corpus
tests in `src/tools/bash/filter/corpus.rs`, built on the shared measurement
core in `src/tools/bench_support.rs` (recipe: the `token-efficiency-benchmark`
skill in `.pi/skills/`). Regenerate the table with:

```
cargo test corpus_benchmark_report -- --nocapture
```

Covers both filter kinds at the seam: structured Rust filters for the top
command classes (cargo test/build/check/clippy, git status/log/diff, npm/pnpm
test — PR 2 of #336) and the declarative TOML pipelines for the long tail
(PR 1).

| class | tokens before | tokens after | reduction | via |
|---|---|---|---|---|
| cargo test (pass) | 439 | 26 | 94% | cargo-test |
| cargo test (pass, workspace) | 342 | 50 | 85% | cargo-test |
| cargo test (fail) | 255 | 183 | 28% | cargo-test |
| cargo build (compile error) | 98 | 84 | 14% | cargo-build |
| cargo build (pass) | 62 | 1 | 98% | cargo-build |
| cargo check (warnings) | 172 | 85 | 51% | cargo-build |
| git status | 141 | 70 | 50% | git-status |
| git diff (source-heavy) | 6482 | 5844 | 10% | git-diff |
| git diff (lockfile churn) | 401 | 169 | 58% | git-diff |
| git log | 792 | 303 | 62% | git-log |
| git log --oneline | 282 | 282 | 0% | (passthrough) |
| npm test (pass) | 95 | 30 | 68% | npm-test |
| npm test (fail) | 414 | 250 | 40% | npm-test |
| vitest (pass) | 50 | 15 | 70% | npm-test |
| vitest (fail) | 339 | 322 | 5% | npm-test |
| npm install (installer log) | 414 | 86 | 79% | npm-install |
| shellcheck (linter) | 408 | 403 | 1% | shellcheck |

## Reading the table

- **Asserted bars** (`corpus_noisy_classes_hit_reduction_bar`): cargo test
  pass ≥ 85 (both samples), cargo build pass ≥ 60, git status ≥ 40, git log
  ≥ 60, git diff (lockfile churn) ≥ 30, npm test pass ≥ 60, vitest pass ≥ 60,
  npm install ≥ 60. Bars are minimums, never exact figures.
- **Failure classes** reduce less by design: failure detail is exempt from
  reduction. Every error message, `file:line`, failing test name, panic
  message, and assertion diff in the corpus survives verbatim — asserted by
  `corpus_failure_and_summary_content_survives_verbatim`.
- **git diff** reduces only machine-generated lockfile churn (`Cargo.lock`,
  `package-lock.json`, `pnpm-lock.yaml`) to a stat line; source hunks are
  signal and stay verbatim (needle-asserted), so a source-heavy diff reduces
  little (10% here) and a diff that would not shrink passes through raw.
- **git log --oneline** passes through untouched: already compact, and the
  structured filter only summarizes the default long format it can parse
  confidently.
- **shellcheck** output is dense signal (findings + caret context); the
  declarative filter only drops blanks. Expected, not a regression.

## Measurement conditions

Debug build, warmed filter registry, best-of-three timing.
`corpus_filter_overhead_under_10ms_per_call` asserts < 10 ms per call on every
corpus sample (registry compile cost excluded as one-time lazy-init).
Observed well under 1 ms per call on the corpus.
