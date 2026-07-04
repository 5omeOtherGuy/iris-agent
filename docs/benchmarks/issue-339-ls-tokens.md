# Issue #339 ls token efficiency — benchmark

Measured over captured real `ls -la` listings in `src/tools/ls_corpus/`
(`flat_many.txt`: a directory of ~66 `.toml` files plus a LICENSE and a NOTICE;
`mixed.txt`: source `.rs` files interleaved with subdirectories). Tokens are
estimated at 4 bytes/token; only the ratios matter. The contracts and bars below
(iris <= rtk with verbatim survival; >= 60% reduction over raw) are asserted by
the tests in `src/tools/ls_corpus/corpus.rs`, built on the shared measurement
core in `src/tools/bench_support.rs` (recipe: the `token-efficiency-benchmark`
skill in `.pi/skills/`); the exact table values are regenerated snapshots, not
asserted figures. Regenerate the tables with:

```
cargo test --bin iris ls_benchmark_report -- --nocapture
```

Three forms of the same directory are compared:

- **raw** — the captured `ls -la` output, what a naive relay would return.
- **rtk** — the RTK-cleaned form (`<octal-perms> <human-size> <name>` per
  entry, `total`/`.`/`..` dropped), modeled in the corpus rather than shelling
  out to RTK.
- **iris** — the real tool output over a reconstructed directory, measured
  through the production seam (`ls`): names only, directories first, `/` suffix.

## raw `ls -la` -> Iris default (names-only)

| class | tokens before | tokens after | reduction | via |
|---|---|---|---|---|
| ls -la (flat, many .toml) | 1225 | 235 | 81% | names-only |
| ls -la (mixed dirs+files) | 353 | 46 | 87% | names-only |

## RTK-cleaned -> Iris default (the "at or above RTK" claim)

| class | tokens before | tokens after | reduction | via |
|---|---|---|---|---|
| ls -la (flat, many .toml) | 427 | 235 | 45% | drop perms+size |
| ls -la (mixed dirs+files) | 97 | 46 | 53% | drop perms+size |

## Reading the tables

- **At or above RTK, confirmed.** Iris's default listing carries 45–53% fewer
  tokens than RTK's cleaned output on these fixtures, because Iris never emits
  the octal-permission and size columns RTK keeps. `corpus_iris_is_at_or_above_rtk`
  asserts parity-or-better (iris is never larger than rtk) and that sampled real
  entry names survive verbatim (zero quality loss). The RTK baseline drops the
  `/` suffix, so the comparison is conservative — Iris pays for dir suffixes and
  still wins.
- **Large cut over raw `ls -la`.** Against the unfiltered listing a relay would
  return, names-only removes the permission, link-count, owner, group, size, and
  date columns plus the `total` header and `.`/`..`, an 81–87% reduction.
  `corpus_iris_cuts_raw_ls_la` asserts a >= 60% minimum bar with every name
  preserved.
- **Bars are minimums, never exact figures.** The 60% bar (raw) and the
  parity-or-better contract (RTK) are what the tests enforce; the 81% and 45%
  are snapshots of the current fixtures.

## Truncation summary (the actionability win)

Token width is only half of ADR-0036. When the entry cap (500) or the byte/line
rail truncates a listing, Iris now ends the output with an exact, actionable
summary instead of a bare `[output truncated]` that forced a blind re-list:

```
adir/
bdir/
cdir/
f0.rs

[11 entries: 3 dirs, 8 files; 4 shown, 7 omitted]
omitted ext: .rs (5), .txt (2)
```

The summary carries the exact total with its dirs/files split, the
shown/omitted counts, and the dominant file extensions among the omitted entries
(directories carry no extension; extensionless files group under `(no ext)`), so
the model knows what was cut without re-listing. In tree mode the counts span
the whole depth-bounded walk, not just the shown prefix. To keep a
model-supplied deep recursive request from traversing an unbounded tree, the
walk stops at a fixed scan budget (10,000 entries); past it the total is a lower
bound and the summary reads `[>=N entries (scan capped): ...]`. These are asserted by
`ls_truncation_appends_summary_with_totals_and_ext`,
`ls_summary_labels_extensionless_files`, and
`ls_tree_truncation_summary_counts_full_tree` in `src/tools/ls.rs`; an under-cap
listing stays byte-identical to the historical output
(`ls_under_cap_has_no_summary`).

## What is asserted vs. reported

- **Asserted** (tests, the contract): iris <= rtk with verbatim name survival;
  iris >= 60% reduction over raw `ls -la`; per-call overhead under 10 ms; the
  truncation summary carries the exact total, dirs/files split, and correct
  omitted-extension counts; under-cap listings are byte-identical to the prior
  output.
- **Reported** (this doc, a snapshot): the 81%/87% and 45%/53% figures and the
  example summary.

## Measurement conditions

Debug build. Tokens are `bench_support::est_tokens` (4 bytes/token); only ratios
are meaningful. Fixtures are captured real listings, not synthetic worst cases;
`ls -la` owner/group widths reflect the capture host and are fixed by the
committed fixture.
