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

## Slice 2 G4 smoke — 2026-07-10

Base: `b446085` plus the slice 2 worktree. This slice changes persistence
cadence, not trigger or compaction behavior, so the exit check used one live
session per lane. Regeneration command:

```sh
IRIS_BENCH_LIVE=1 IRIS_AUTO_COMPACTION_SESSIONS=1 \
  cargo test --locked auto_compaction_live_loop_ -- \
  --ignored --nocapture --test-threads=1
```

Parent turns used the two protocol lanes. Every model-backed summary used
`anthropic/claude-opus-4-6` with medium thinking. Both sessions forced two real
compactions and rebuilt byte-identically after exit; no session was excluded.
G1 remains inapplicable until slice 3.

| lane | sessions | compactions | worst post-apply/start | G2 | G3 | G4 | G5 | real reads | exclusions |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `anthropic/claude-haiku-4-5` | 1 | 2 | 16,948 / 21,299 (79.6%) | 1/1 | 1/1 | 1/1 | 1/1 | 1/1 | 0 |
| `openai-codex/gpt-5.4-mini` | 1 | 2 | 16,166 / 21,299 (75.9%) | 1/1 | 1/1 | 1/1 | 1/1 | 1/1 | 0 |

The run completed in 103.30 seconds. No provider, auth, tool, worker,
persistence, or resume errors occurred.

## Slice 3 passing run — 2026-07-10

Base: `95d7bc2` plus the slice 3 worktree. Synthetic effective window: 32,768
tokens. Start threshold: 21,299 tokens. Parent turns used the two protocol
lanes. Every model-backed summary used `anthropic/claude-opus-4-6` with medium
thinking.

Regeneration command:

```sh
IRIS_BENCH_LIVE=1 IRIS_AUTO_COMPACTION_SESSIONS=10 \
  cargo test --locked auto_compaction_live_loop_ -- \
  --ignored --nocapture --test-threads=1
```

G1 is active in this slice. For each compaction lifecycle/apply event inside a
continuing turn, the instrument measures the gap to the next
`ProviderTurnStarted` event. It excludes hard-tier boundaries and post-turn
events with no next request in that turn. G2–G5 retain the slice-1 definitions.

| lane | sessions | compactions | worst G1 non-hard block | worst post-apply/start | G2 | G3 | G4 | G5 | real reads | exclusions |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `anthropic/claude-haiku-4-5` | 10 | 20 | 1.2 ms / 200 ms (0.6%) | 16,958 / 21,299 (79.6%) | 10/10 | 10/10 | 10/10 | 10/10 | 10/10 | 0 |
| `openai-codex/gpt-5.4-mini` | 10 | 20 | 2.7 ms / 200 ms (1.4%) | 18,277 / 21,299 (85.8%) | 10/10 | 10/10 | 10/10 | 10/10 | 10/10 | 0 |

Per-session evidence:

| lane | session | compactions | G1 block | maximum post-apply | G3 needle/marker/carry | G4 | G5 | read | error |
|---|---:|---:|---:|---:|---|---|---|---|---|
| Anthropic | 00 | 2 | 0.8 ms | 16,872 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 01 | 2 | 0.7 ms | 16,948 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 02 | 2 | 0.9 ms | 16,848 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 03 | 2 | 1.2 ms | 16,824 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 04 | 2 | 0.8 ms | 16,823 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 05 | 2 | 0.7 ms | 16,958 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 06 | 2 | 0.7 ms | 16,904 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 07 | 2 | 1.1 ms | 16,856 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 08 | 2 | 0.8 ms | 16,858 | pass/pass/pass | pass | pass | pass | — |
| Anthropic | 09 | 2 | 0.7 ms | 16,839 | pass/pass/pass | pass | pass | pass | — |
| Codex | 00 | 2 | 2.7 ms | 18,277 | pass/pass/pass | pass | pass | pass | — |
| Codex | 01 | 2 | 2.6 ms | 15,115 | pass/pass/pass | pass | pass | pass | — |
| Codex | 02 | 2 | 2.1 ms | 15,042 | pass/pass/pass | pass | pass | pass | — |
| Codex | 03 | 2 | 1.9 ms | 12,581 | pass/pass/pass | pass | pass | pass | — |
| Codex | 04 | 2 | 2.1 ms | 12,831 | pass/pass/pass | pass | pass | pass | — |
| Codex | 05 | 2 | 2.1 ms | 14,006 | pass/pass/pass | pass | pass | pass | — |
| Codex | 06 | 2 | 1.1 ms | 16,885 | pass/pass/pass | pass | pass | pass | — |
| Codex | 07 | 2 | 1.8 ms | 14,563 | pass/pass/pass | pass | pass | pass | — |
| Codex | 08 | 2 | 2.1 ms | 14,471 | pass/pass/pass | pass | pass | pass | — |
| Codex | 09 | 2 | 2.2 ms | 16,321 | pass/pass/pass | pass | pass | pass | — |

No session was excluded. The full run completed in 1,120.02 seconds. No
provider, auth, tool, worker, persistence, or resume errors occurred.

Post-fix smoke after moving the bounded hard wait off the current-thread loop
and making it cancellation-raceable used the same command with
`IRIS_AUTO_COMPACTION_SESSIONS=1`. Anthropic passed with two compactions, G1
0.8 ms, post-apply 16,899/21,299, and G2–G5 plus the real read all green. The
Codex row was excluded before a session ran because the lane returned:

```text
Codex request failed [status=429 endpoint=/codex/responses model=gpt-5.4-mini error_type=usage_limit_reached]
```

The smoke had one exclusion and no fabricated metrics. The preceding full
Codex run remains the slice result; the post-run change affects only hard-wait
cancellation and the controller's borrow lifetime, while the full run's
non-hard G1 path is unchanged.

## Slice 4 worker-v2 run — 2026-07-10

Base: `d9c43dc` plus the slice 4 worktree. Worker input is the new verbatim
`transcript` default. Parent traffic used the protocol lanes; every summary
worker used `anthropic/claude-opus-4-6` with medium thinking. The harness now
reports the specified summarization cache-hit ratio directly from persisted
worker usage: `cacheReadInputTokens / inputTokens`. A zero input denominator is
`unknown`, never zero.

Regeneration command:

```sh
IRIS_BENCH_LIVE=1 IRIS_AUTO_COMPACTION_SESSIONS=10 \
  cargo test --locked auto_compaction_live_loop_ -- \
  --ignored --nocapture --test-threads=1
```

| lane | sessions | compactions | worst G1 | worst post-apply/start | G2 | G3 | G4 | G5 | reads | cache-hit observations | exclusions |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|
| `anthropic/claude-haiku-4-5` | 10 | 21 | 15.0 ms | 17,133 / 21,299 (80.4%) | 10/10 | 10/10 | 10/10 | 10/10 | 10/10 | 8 × 0.000; 2 × unknown | 0 |
| `openai-codex/gpt-5.4-mini` | 10 | 20 | 21.8 ms | 16,413 / 21,299 (77.1%) | 10/10 | 10/10 | 10/10 | 10/10 | 10/10 | 1 × 0.462; 9 × 0.000 | 0 |

Haiku per-session evidence:

| session | compactions | G1 | maximum post-apply | G2–G5/read | worker cache hit |
|---:|---:|---:|---:|---|---:|
| 00 | 2 | 15.0 ms | 17,083 | pass | unknown |
| 01 | 2 | 0.8 ms | 17,108 | pass | unknown |
| 02 | 2 | 0.7 ms | 16,865 | pass | 0.000 |
| 03 | 2 | 2.1 ms | 16,907 | pass | 0.000 |
| 04 | 2 | 0.7 ms | 17,090 | pass | 0.000 |
| 05 | 2 | 1.0 ms | 16,908 | pass | 0.000 |
| 06 | 2 | 1.1 ms | 16,922 | pass | 0.000 |
| 07 | 3 | 0.7 ms | 17,133 | pass | 0.000 |
| 08 | 2 | 0.7 ms | 17,086 | pass | 0.000 |
| 09 | 2 | 0.7 ms | 16,917 | pass | 0.000 |

Codex per-session evidence:

| session | compactions | G1 | maximum post-apply | G2–G5/read | worker cache hit |
|---:|---:|---:|---:|---|---:|
| 00 | 2 | 2.7 ms | 13,882 | pass | 0.462 |
| 01 | 2 | 2.7 ms | 16,413 | pass | 0.000 |
| 02 | 2 | 21.8 ms | 14,640 | pass | 0.000 |
| 03 | 2 | 3.3 ms | 14,685 | pass | 0.000 |
| 04 | 2 | 2.6 ms | 15,052 | pass | 0.000 |
| 05 | 2 | 16.1 ms | 14,680 | pass | 0.000 |
| 06 | 2 | 1.2 ms | 12,948 | pass | 0.000 |
| 07 | 2 | 17.6 ms | 10,784 | pass | 0.000 |
| 08 | 2 | 2.3 ms | 14,495 | pass | 0.000 |
| 09 | 2 | 2.0 ms | 15,932 | pass | 0.000 |

The Haiku run completed in 400.57 seconds and the Codex run in 745.95 seconds.
Neither full run had a provider, auth, tool, worker, persistence, or resume
error. The lanes were run separately against the same worker-v2 behavior after
an account quota interruption. Preliminary Codex probes recorded this error
verbatim before the quota reopened:

```text
Codex request failed [status=429 endpoint=/codex/responses model=gpt-5.4-mini error_type=usage_limit_reached]
```

Those probes are not averaged into the passing table. After the quota reopened,
a one-session probe passed with two compactions before the full 10-session
Codex run above.

## Slice 5 reactive-recovery run — 2026-07-10

Base: `b20cec7` plus the slice 5 worktree. Parent traffic used the two protocol
lanes. Every model-backed summary used `anthropic/claude-opus-4-6` with medium
thinking. The new induced variant injects one typed overflow before forwarding
to the real provider. Recovery itself is deterministic excerpts, so no summary
worker request is required by that variant.

Regeneration commands:

```sh
IRIS_BENCH_LIVE=1 cargo test --locked \
  auto_compaction_reactive_overflow_ -- \
  --ignored --nocapture --test-threads=1

IRIS_BENCH_LIVE=1 IRIS_AUTO_COMPACTION_SESSIONS=10 \
  cargo test --locked auto_compaction_live_loop_ -- \
  --ignored --nocapture --test-threads=1
```

Induced-overflow evidence:

| lane | injected overflows | real forwarded requests | provider starts | excerpts applied | G4 exact | G5 | real read |
|---|---:|---:|---:|---:|---:|---:|---:|
| `anthropic/claude-haiku-4-5` | 1 | 2 | 3 | pass | pass | pass | pass |
| `openai-codex/gpt-5.4-mini` | 1 | 2 | 3 | pass | pass | pass | pass |

Two forwarded requests are expected: the resent request performs the real read,
then the real provider receives the tool result. The first instrument attempt
used a 20,000-token retained tail, leaving no pair-safe range in the synthetic
seed. It was rejected rather than averaged into the passing evidence. Both
lanes returned:

```text
provider rejected context after bounded reactive recovery: measured ~21798 tokens against a 131072-token window; try `/compact <focus>`, `/new`, or switch model
```

The variant now uses the specified 1,000-token deep-cut target and passed on
both lanes.

Normal live-loop summary:

| lane | scripted sessions | evaluated | compactions | worst G1 | worst post-apply/start | G2 | G3 | G4 | G5 | reads | cache-hit observations | exclusions |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|
| `anthropic/claude-haiku-4-5` | 10 | 10 | 23 | 3.5 ms | 17,130 / 21,299 (80.4%) | 10/10 | 10/10 | 10/10 | 10/10 | 10/10 | 10 × 0.000 | 0 |
| `openai-codex/gpt-5.4-mini` | 10 | 9 | 18 | 22.4 ms | 18,934 / 21,299 (88.9%) | 9/9 | 9/9 | 9/9 | 9/9 | 9/9 | 9 × 0.000 | 1 |

Haiku per-session evidence:

| session | compactions | G1 | maximum post-apply | G2–G5/read | worker cache hit | error |
|---:|---:|---:|---:|---|---:|---|
| 00 | 2 | 1.1 ms | 16,952 | pass | 0.000 | — |
| 01 | 2 | 1.5 ms | 16,894 | pass | 0.000 | — |
| 02 | 2 | 2.8 ms | 16,857 | pass | 0.000 | — |
| 03 | 3 | 3.5 ms | 17,130 | pass | 0.000 | — |
| 04 | 3 | 1.7 ms | 16,938 | pass | 0.000 | — |
| 05 | 2 | 1.6 ms | 16,958 | pass | 0.000 | — |
| 06 | 3 | 1.3 ms | 16,927 | pass | 0.000 | — |
| 07 | 2 | 1.2 ms | 16,855 | pass | 0.000 | — |
| 08 | 2 | 1.1 ms | 16,909 | pass | 0.000 | — |
| 09 | 2 | 1.9 ms | 16,905 | pass | 0.000 | — |

Codex per-session evidence:

| session | compactions | G1 | maximum post-apply | G2–G5/read | worker cache hit | error |
|---:|---:|---:|---:|---|---:|---|
| 00 | 2 | 2.1 ms | 12,371 | pass | 0.000 | — |
| 01 | 2 | 2.6 ms | 17,267 | pass | 0.000 | — |
| 02 | — | — | — | excluded | — | `provider stream produced no events for 90s` |
| 03 | 2 | 22.4 ms | 14,636 | pass | 0.000 | — |
| 04 | 2 | 2.7 ms | 16,189 | pass | 0.000 | — |
| 05 | 2 | 2.3 ms | 18,934 | pass | 0.000 | — |
| 06 | 2 | 2.1 ms | 16,901 | pass | 0.000 | — |
| 07 | 2 | 2.2 ms | 17,285 | pass | 0.000 | — |
| 08 | 2 | 1.4 ms | 17,299 | pass | 0.000 | — |
| 09 | 2 | 15.4 ms | 15,160 | pass | 0.000 | — |

The normal pair completed in 2,368.77 seconds. Codex session 02 took an
unusually long provider path before the worker reported the 90-second stream
idle error above. That latency is external to G1, which measures only
compaction-event-to-next-request blocking. No evaluated session had a turn,
tool, persistence, recall, metadata, or resume failure.

## Slice 7 provider-native probe — 2026-07-10

Base: `1e1ec0d` plus the slice 7 worktree. The portable fallback worker remains
`anthropic/claude-opus-4-6` with medium thinking. The native probe uses an
80,000-token effective window because Anthropic documents a 50,000-token
minimum compact trigger. It plants a unique needle, performs a real `read`,
exits, and compares live with resumed context.

Regeneration command:

```sh
IRIS_BENCH_LIVE=1 cargo test --locked \
  auto_compaction_native_live_anthropic -- \
  --ignored --nocapture --test-threads=1
```

| lane | native applied | opaque blocks | usage | needle | live==resumed | result |
|---|---:|---:|---:|---:|---:|---|
| `anthropic/claude-haiku-4-5` | 0 | 0 | unknown | pass | pass | provider capability rejected |

The first attempt exposed and then pinned a worker-runtime bug. Its panic was:

```text
Cannot drop a runtime in a context where blocking is not allowed. This happens when a runtime is dropped from within an asynchronous context.
```

After the runtime fix, the request reached the live provider and failed with:

```text
background compaction failed; using deterministic fallback: Anthropic native compaction request failed (status=400, error_type=invalid_request_error)
```

The session then applied one deterministic excerpts entry, retained
`--enable-zeta`, and rebuilt byte-exactly. This is a failed native capability
probe, not a passing native row and not an excluded ordinary-LVP session.
`compaction.providerNative` therefore remains default-off. No success metric is
inferred from the fallback.

OpenAI v2 capability was probed separately because it cannot yet produce a
portable Iris entry:

```sh
IRIS_BENCH_LIVE=1 IRIS_OPENAI_NATIVE_COMPACTION_PROBE=1 \
  cargo test --locked auto_compaction_native_probe_codex -- \
  --ignored --nocapture --test-threads=1
```

| lane | request shape | opaque blocks | portable text | backend capability | Iris route |
|---|---|---:|---:|---:|---|
| `openai-codex/gpt-5.4-mini` | v2 `compaction_trigger` | 1 | no | pass | rejected by portable-text invariant |

The live output was:

```text
OPENAI NATIVE PROBE lane=openai-codex/gpt-5.4-mini adapter=openai-codex-responses model=gpt-5.4-mini blocks=1 portable_text=false
```

This proves backend support without claiming a durable provider-native entry.

The first ordinary Codex LVP attempt was stopped after sessions 00 and 01 both
returned:

```text
Codex request failed [status=429 endpoint=/codex/responses model=gpt-5.4-mini error_type=usage_limit_reached]
```

Two exclusions already exceeded the protocol allowance, so the remaining eight
calls were cancelled. The aborted run contributes no gate metrics and will be
regenerated when quota reopens.

A later one-session availability retry returned the same error before any
session could be evaluated. It also contributes no metrics.

Portable-path Haiku LVP:

| lane | sessions | compactions | worst G1 | worst post-apply/start | G2 | G3 | G4 | G5 | reads | cache-hit observations | exclusions |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|
| `anthropic/claude-haiku-4-5` | 10 | 22 | 16.2 ms | 17,103 / 21,299 (80.3%) | 10/10 | 10/10 | 10/10 | 10/10 | 10/10 | 3 × 0.999; 7 × 0.000 | 0 |

| session | compactions | G1 | maximum post-apply | G2–G5/read | worker cache hit | error |
|---:|---:|---:|---:|---|---:|---|
| 00 | 3 | 15.5 ms | 17,051 | pass | 0.999 | — |
| 01 | 2 | 10.7 ms | 17,090 | pass | 0.999 | — |
| 02 | 3 | 16.2 ms | 17,081 | pass | 0.999 | — |
| 03 | 2 | 3.1 ms | 17,084 | pass | 0.000 | — |
| 04 | 2 | 8.7 ms | 17,098 | pass | 0.000 | — |
| 05 | 2 | 1.6 ms | 17,103 | pass | 0.000 | — |
| 06 | 2 | 0.9 ms | 17,070 | pass | 0.000 | — |
| 07 | 2 | 1.3 ms | 17,090 | pass | 0.000 | — |
| 08 | 2 | 8.4 ms | 17,088 | pass | 0.000 | — |
| 09 | 2 | 0.8 ms | 17,040 | pass | 0.000 | — |

The run completed in 426.37 seconds. Every model-backed summary used
`anthropic/claude-opus-4-6` with medium thinking. No provider, auth, tool,
worker, persistence, recall, metadata, or resume error occurred.

## Slice 8 model-tool verification — 2026-07-10

Live protocol: not applicable. `compaction.modelTool` is default-off, and the
scripted LVP neither enables nor calls model-only control tools. Running it
would regenerate the unchanged default path and add provider cost without
exercising slice 8.

Deterministic coverage drives the tool itself, verifies the exact
scheduled-not-done result, rejects non-empty arguments before setting state,
and proves the one-shot request creates no entry until the next pair-closed
governor boundary. That boundary compacts successfully even with automatic
thresholds disabled. The final post-slice-9 LVP pair will cover the accumulated
default behavior.

## Slice 9 tuning probes — 2026-07-10

The instrument now reconstructs the true message-context size before every
apply from durable event fields:

```text
reclaimed = originalTokensEstimate - summaryTokensEstimate
before = contextTokensAfterApply + reclaimed
```

It reports covered-range reduction and total-context reduction separately. The
earlier `maximum post-apply/start` column never claimed the reduction delta and
cannot diagnose a small eligible prefix by itself.

One-session Haiku candidate probes used the same scripted LVP and Opus 4.6
medium worker:

| policy | compactions | shallowest before -> after | reclaimed | covered | total | worst G1 | G2–G5/read |
|---|---:|---:|---:|---:|---:|---:|---|
| 0.55/0.65/0.85, keep 20k | 3 | 19,820 -> 18,319 | 1,501 | 83.4% | 7.6% | below 200 ms | pass |
| 0.60/0.72/0.90, keep 6k | 2 | 28,464 -> 13,809 | 14,655 | 96.1% | 51.5% | 1.5 ms | pass |
| 0.60/0.72/0.90, keep 8k | 2 | 30,428 -> 15,487 | 14,941 | 97.4% | 49.1% | 1.7 ms | pass |

The old worker compacted its eligible prefix by 83.4%; it did not write an
oversized summary. The 20k retained-tail target left most recent pair-closed
tool groups outside that prefix. Starting at 65% also selected the range before
it grew. The selected 8k policy keeps 1,678 more recent tokens than 6k while
still removing 49.1% of the full message context.

The live loop also pairs parent usage across applies. Anthropic rows use
provider-reported cache writes. Codex rows are explicitly labeled
`derived-fresh-input` and use `input - cache_read`, because that lane does not
report writes.

A Codex availability smoke on the selected defaults produced no eligible row
and recorded the backend error verbatim:

```text
Codex request failed [status=429 endpoint=/codex/responses model=gpt-5.4-mini error_type=usage_limit_reached]
```

It is not a passing protocol run and contributes no metrics. The final two
10-session pairs remain pending until Codex traffic evaluates.

Selected-default Haiku full run:

| lane | sessions | compactions | worst G1 | worst post/start | shallowest apply | covered / total reduction | G2–G5/read | worker cache | parent reported write, pre -> post | exclusions |
|---|---:|---:|---:|---:|---:|---:|---|---|---:|---:|
| `anthropic/claude-haiku-4-5` | 10 | 23 | 16.7 ms | 19,446/23,592 | 22,391 -> 16,827 | 91.7% / 24.8% | 10/10 | 9 × 0.000; 1 unknown | 45,012 -> 442,524 (9.831×) | 0 |

| session | compactions | G1 | maximum post | shallowest before -> after | covered / total | G2–G5/read | worker hit | parent write pre -> post / pairs |
|---:|---:|---:|---:|---:|---:|---|---:|---:|
| 00 | 2 | 2.0 ms | 15,471 | 26,009 -> 13,906 | 96.3% / 46.5% | pass | 0.000 | 2,716 -> 37,015 / 2 |
| 01 | 2 | 1.8 ms | 19,446 | 31,572 -> 19,446 | 96.6% / 38.4% | pass | 0.000 | 2,716 -> 43,398 / 2 |
| 02 | 2 | 3.1 ms | 15,571 | 27,658 -> 15,571 | 96.1% / 43.7% | pass | 0.000 | 4,830 -> 37,160 / 2 |
| 03 | 3 | 15.9 ms | 16,827 | 22,391 -> 16,827 | 91.7% / 24.8% | pass | 0.000 | 5,093 -> 63,222 / 3 |
| 04 | 2 | 1.8 ms | 16,310 | 28,361 -> 16,310 | 95.9% / 42.5% | pass | 0.000 | 2,712 -> 38,997 / 2 |
| 05 | 2 | 2.7 ms | 15,493 | 25,888 -> 13,837 | 95.9% / 46.6% | pass | 0.000 | 2,684 -> 37,147 / 2 |
| 06 | 3 | 16.7 ms | 15,516 | 19,020 -> 13,228 | 95.3% / 30.5% | pass | 0.000 | 3,819 -> 58,452 / 3 |
| 07 | 3 | 13.5 ms | 16,142 | 21,949 -> 16,142 | 95.2% / 26.5% | pass | 0.000 | 5,691 -> 55,479 / 3 |
| 08 | 2 | 2.5 ms | 16,569 | 28,622 -> 16,569 | 95.9% / 42.1% | pass | unknown | 2,707 -> 39,515 / 2 |
| 09 | 2 | 1.2 ms | 15,456 | 30,410 -> 15,456 | 97.5% / 49.2% | pass | 0.000 | 12,044 -> 32,139 / 2 |

The run completed in 620.38 seconds. `unknown` means the lane returned a
non-null usage object with no positive input count; no hit rate is inferred.
Every entry still passed G5 and identified the required Opus worker. The
parent-write ratio is cache cost after a prefix rewrite, not context
reclamation.
