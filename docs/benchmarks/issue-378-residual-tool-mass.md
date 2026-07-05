# Residual tool-result mass — microcompaction gate (#378, ADR-0048 task 0)

ADR-0048 rule 5 (ADR-0036 "reduction is measured") gates the fold engine on this
report: measure the residual tool-result mass before building, and re-scope the
v1 slice if superseded reads do not dominate. This is a read-only measurement —
no `src/` behavior change.

## Regeneration

```
python3 scripts/measure-residual-tool-mass.py [SESSION_ROOT]
```

`SESSION_ROOT` defaults to `$IRIS_SESSION_DIR`, else `~/.iris/sessions`. The
script scans every `**/*.jsonl` transcript under the root and reads each entry's
persisted `tokenEstimate` (`src/session.rs`) directly — token mass is never
re-estimated.

## Corpus

Real Iris session transcripts from one developer machine's local store
(`~/.iris/sessions`), spanning multiple workspaces (iris-agent worktrees,
mistral4pi, drafts, home). Not a synthetic corpus. Absolute figures are
machine-specific and will differ per store; the method and the ratios are the
reproducible artifact.

- transcripts scanned: 319
- sessions with tool results: 146
- total tool-result token mass: 1,643,997

## Measured breakdown

By tool class (share of total tool-result mass; `okFalse` = execution-level
failures, `ok:false`):

| class | mass | share | count | okFalse |
|---|---|---|---|---|
| bash | 698,934 | 42.5% | 903 | 35 |
| read | 641,693 | 39.0% | 559 | 8 |
| grep | 248,911 | 15.1% | 341 | 5 |
| find | 22,951 | 1.4% | 110 | 2 |
| ls | 18,501 | 1.1% | 149 | 3 |
| edit | 11,448 | 0.7% | 280 | 15 |
| write | 1,559 | 0.1% | 84 | 1 |

By age (per-session tool-result deciles, oldest to newest): mass is roughly flat
across age (11-15% in the oldest three deciles, 6-10% in the rest). Tool-result
mass accrues steadily over a session; it is not concentrated in either the tail
or the head.

Spent mass (ADR-0048 v1 scope — provably superseded, detected per session):

| spent class | mass | share of total |
|---|---|---|
| superseded reads (read/ls, latest-read-wins) | 295,643 | 18.0% |
| retired command output (grep/find/bash, identical-rerun proxy / upper bound) | 25,239 | 1.5% |
| **combined foldable** | **320,882** | **19.5%** |

Sensitivity:

- Foldable share of the residual (oldest 75% of each session, modelling a
  never-folded tail): 22.8%.
- Long sessions (>=40 tool results, n=16 — where residency actually
  accumulates): foldable share 32.3%.

Detection notes and measurement limits:

- Superseded reads use latest-read-wins: a `read`/`ls` whose path is later read,
  edited, or written again. This is the clean, conservative signal.
- Retired command output counts every *identical*-args earlier copy re-run
  later, regardless of whether that earlier copy failed. It is therefore an
  **identical-rerun command-output proxy**, not a failure-output measurement:
  as an answer to the DoD's "retired failure output" question it is an **upper
  bound**, because it includes successful reruns. Bash results do not persist an
  exit code (only an `ok` execution flag), so true exit-status failures are not
  measurable from current transcripts; a failing-then-passing test loop is only
  caught when the command string is byte-identical across runs, and real
  red-green loops that vary the command (`| tail -30` -> `| tail -50`) are
  missed entirely (so the proxy also undercounts total reruns). Either way the
  figure is negligible and cannot approach the superseded-read figure:
  `ok:false` results total 68 across the whole corpus and carry negligible mass.

## Verdict

**Do superseded reads + retired failure output dominate the residual
tool-result mass? No.** Combined they are 19.5% of all tool-result mass, 22.8%
of the residual after excluding a retained tail, and 32.3% even in the long
sessions where accumulation is worst — never a majority. Within that foldable
slice, superseded reads are effectively the entire signal (18.0% of 19.5%);
retired failure output is negligible (1.5% — and that figure is an
identical-rerun *upper bound* for failure output, inflated by successful
reruns, since bash exit status is not persisted and true failure output is not
directly measurable). The dominant residual mass is single-use `bash` output
(42.5%) and reads that are never re-touched (most of the 39.0% `read` mass) —
results that were load-bearing once and are neither superseded nor reproducible-
by-args, so they sit outside the ADR-0048 "spent" definition entirely.

### Re-scope recommendation for the #378 v1 slice

1. **Center v1 on superseded reads (latest-read-wins) only.** They carry the
   whole detectable spent signal and recover for free from the workspace (the
   stub names the path). This is the highest-value, lowest-risk fold class and
   should ship first.
2. **Drop retired failure output from the v1 slice.** It is negligible in this
   corpus (1.5%, an identical-rerun upper-bound proxy) and the persisted data
   lacks a bash exit code to classify failure reliably — the guard against folding
   unresolved failures (ADR-0040) would be doing real work to protect a class
   that reclaims almost nothing. Defer it until a corpus with heavy red-green
   test loops shows it earns its guard complexity.
3. **Set expectations honestly.** Even a perfect superseded-read fold reclaims
   roughly one fifth of tool-result mass overall (about one third in long
   sessions). That is a real, deterministic, lossless win and justifies v1 — but
   it does not make full compaction (ADR-0009/0041) redundant. If a larger cut
   is the goal, the lever is the single-use `bash`/`read` majority, which needs
   an age-based fold of reproducible results (recoverable from the workspace),
   not a "spent"-only policy — a separate, broader slice with its own ADR-0045
   needle validation, out of scope for this gate.
