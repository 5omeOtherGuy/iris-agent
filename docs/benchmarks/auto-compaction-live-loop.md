# Auto-compaction live loop

Real-provider protocol for the auto-compaction program. It uses the production
session, tool, worker, entry, rebuild, and resume seams. CI never runs it:
every test is both `#[ignore]` and gated by `IRIS_BENCH_LIVE=1`.

Regenerate the two-lane protocol sequentially so rows cannot interleave:

```sh
IRIS_BENCH_LIVE=1 IRIS_AUTO_COMPACTION_SESSIONS=10 \
  cargo test --locked auto_compaction_live_loop_ -- \
  --ignored --nocapture --test-threads=1
```

Use only `anthropic/claude-haiku-4-5` and
`openai-codex/gpt-5.4-mini` for parent turns. Every summarization subagent uses
`anthropic/claude-opus-4-6` with medium thinking. Override the session count only
for instrument smoke checks. Each session plants a unique flag, forces real
repository reads, forces at least two compactions, probes the flag, then reopens
the session and compares the rebuilt context with the final live context. G5
also checks that every entry records the required Opus worker lane.

## Slice 0 baseline — 2026-07-10

Base: `3f4c5c2`. Budget: 8,000 estimated tokens. The hard tier and governor do
not exist in this slice, so G1 is not applicable. G2 uses the maximum context
estimate recorded immediately after apply. G3 records the final needle answer,
recall marker, and deterministic carry block. G4 compares bytes covering every
message field. G5 requires `origin` and `workerUsage` on every
entry; model-backed entries require non-null usage.

| lane | sessions | compactions | compactions/session | worst post-apply | G2 | G3 | G4 | G5 | real reads | exclusions |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `anthropic/claude-haiku-4-5` | 10 | 30 | 3.0 | 6,995 / 8,000 (87.4%) | 10/10 | 10/10 | 10/10 | 10/10 | 10/10 | 0 |
| `openai-codex/gpt-5.4-mini` | 10 | 24 | 2.4 | 6,820 / 8,000 (85.3%) | 10/10 | 10/10 | 10/10 | 10/10 | 10/10 | 0 |

Per-session evidence:

| lane | session | compactions | maximum post-apply | G3 needle/marker/carry | G4 | G5 | read | error |
|---|---:|---:|---:|---|---|---|---|---|
| Anthropic | 00 | 3 | 6,934 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 01 | 3 | 6,973 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 02 | 3 | 6,907 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 03 | 3 | 6,882 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 04 | 3 | 6,924 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 05 | 3 | 6,876 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 06 | 3 | 6,953 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 07 | 3 | 6,934 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 08 | 3 | 6,831 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 09 | 3 | 6,995 | pass/pass/pass | pass | pass | pass | — |
| Codex | 00 | 2 | 6,601 | pass/pass/pass | pass | pass | pass | — |
| Codex | 01 | 2 | 6,616 | pass/pass/pass | pass | pass | pass | — |
| Codex | 02 | 2 | 6,382 | pass/pass/pass | pass | pass | pass | — |
| Codex | 03 | 2 | 6,672 | pass/pass/pass | pass | pass | pass | — |
| Codex | 04 | 3 | 6,731 | pass/pass/pass | pass | pass | pass | — |
| Codex | 05 | 3 | 6,770 | pass/pass/pass | pass | pass | pass | — |
| Codex | 06 | 2 | 6,724 | pass/pass/pass | pass | pass | pass | — |
| Codex | 07 | 2 | 6,820 | pass/pass/pass | pass | pass | pass | — |
| Codex | 08 | 3 | 6,756 | pass/pass/pass | pass | pass | pass | — |
| Codex | 09 | 3 | 6,801 | pass/pass/pass | pass | pass | pass | — |

No provider, auth, tool, worker, or resume errors occurred. An earlier
concurrent instrument shakeout completed 20/20 sessions but interleaved lane
rows; it is excluded from the table because one G2 row could not be attributed
safely. The sequential run above replaced it rather than averaging it away.

Post-extraction instrument smoke (`IRIS_AUTO_COMPACTION_SESSIONS=1`) tightened
recall-marker accounting to one marker per compaction and switched G4 from a
debug representation to deterministic JSON bytes. Anthropic: 3 compactions,
6,822 maximum post-apply; Codex: 2 compactions, 6,668 maximum post-apply. Both
answered the needle, retained every recall marker plus a carry block, rebuilt
byte-exactly, recorded complete metadata, and executed a real read. No errors
or exclusions.

## Slice 1 attempt 1 — rejected

Date: 2026-07-10. Budget: 12,000 tokens. Start threshold: 7,800 tokens.

Anthropic completed 10/10 sessions with 22 compactions. G2, G3, G4, and G5
passed in every session; worst post-apply context was 7,295 tokens. Two rows
reported `real read = false` because the instrument searched only the final
rebuilt context after a third compaction had moved the read result behind
recall. The provider had executed the read. The instrument now uses the captured
`ToolResult` event. This attempt is rejected rather than corrected in place.

The Codex lane then returned the same quota error for sessions 00 through 03.
The run was stopped to avoid six more known-failing sessions. No Codex row is
eligible, so this is not a passing protocol run or a flaky exclusion:

```text
Codex request failed [status=429 endpoint=/codex/responses model=gpt-5.4-mini error_type=usage_limit_reached]
```

## Slice 1 passing run — 2026-07-10

Base: `e419d1d` plus the slice 1 worktree. Synthetic effective window: 32,768
tokens. Start threshold: 21,299 tokens. This is the smallest window that keeps
model-backed work enabled (`4 * 8,192` summary reserve). Parent turns used the
two protocol lanes. Every model-backed summary used
`anthropic/claude-opus-4-6` with medium thinking; hard-tier deterministic
fallbacks recorded `origin=excerpts` and null worker usage.

G1 is not applicable until slice 3 installs mid-turn governance. G2 uses the
local recomputation immediately after each apply, as specified for a rewritten
context. G3 requires the answer, one recall marker per compaction, and the carry
block. G4 compares serialized live and resumed messages byte-for-byte. G5
requires an origin and usage appropriate to that origin on every entry, and
rejects any model-backed worker not attributed to Opus 4.6.

| lane | sessions | compactions | compactions/session | worst post-apply/start | G2 | G3 | G4 | G5 | real reads | exclusions |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `anthropic/claude-haiku-4-5` | 10 | 20 | 2.0 | 17,005 / 21,299 (79.8%) | 10/10 | 10/10 | 10/10 | 10/10 | 10/10 | 0 |
| `openai-codex/gpt-5.4-mini` | 10 | 20 | 2.0 | 18,310 / 21,299 (86.0%) | 10/10 | 10/10 | 10/10 | 10/10 | 10/10 | 0 |

Per-session evidence:

| lane | session | compactions | maximum post-apply | G3 needle/marker/carry | G4 | G5 | read | error |
|---|---:|---:|---:|---|---|---|---|---|
| Anthropic | 00 | 2 | 16,878 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 01 | 2 | 16,931 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 02 | 2 | 16,863 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 03 | 2 | 16,906 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 04 | 2 | 17,005 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 05 | 2 | 16,852 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 06 | 2 | 16,926 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 07 | 2 | 16,900 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 08 | 2 | 16,888 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 09 | 2 | 16,873 | pass/pass/pass | pass | pass | pass | — |
| Codex | 00 | 2 | 15,040 | pass/pass/pass | pass | pass | pass | — |
| Codex | 01 | 2 | 15,078 | pass/pass/pass | pass | pass | pass | — |
| Codex | 02 | 2 | 12,855 | pass/pass/pass | pass | pass | pass | — |
| Codex | 03 | 2 | 18,310 | pass/pass/pass | pass | pass | pass | — |
| Codex | 04 | 2 | 16,871 | pass/pass/pass | pass | pass | pass | — |
| Codex | 05 | 2 | 18,175 | pass/pass/pass | pass | pass | pass | — |
| Codex | 06 | 2 | 15,054 | pass/pass/pass | pass | pass | pass | — |
| Codex | 07 | 2 | 15,140 | pass/pass/pass | pass | pass | pass | — |
| Codex | 08 | 2 | 14,536 | pass/pass/pass | pass | pass | pass | — |
| Codex | 09 | 2 | 14,354 | pass/pass/pass | pass | pass | pass | — |

No session was excluded. The full run completed in 1,030.02 seconds.
