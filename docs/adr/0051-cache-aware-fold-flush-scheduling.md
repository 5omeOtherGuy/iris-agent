# ADR-0051: Cache-aware fold flush scheduling

**Date**: 2026-07-05
**Status**: accepted
**Deciders**: Iris maintainers, Claude agent session

## Context

ADR-0048's fold engine flushed on one trigger: a token watermark.
The watermark fires under context pressure, which correlates with active
(warm-cache) work — the worst-timed moment. Measured
(`docs/benchmarks/issue-400-fold-flush-cost.md`): a warm fold-only flush costs
2,129 realized cache-write tokens on the live seed (~68-turn payback), while
the same flush at a compaction boundary is free (marginal ≤ 0) and a cold
cache makes it free by construction. Providers force prefix re-reads at known
moments regardless of what Iris does; those moments are free flush windows.

## Decision

Split fold detection from flushing (issue #400). Detection recomputes a
pending-fold set at every turn boundary — pure, in-memory, derived from the
transcript, no new persistence. Flushing waits for a trigger, in priority
order:

| Class | Trigger | Why it is free |
|---|---|---|
| A1 | compaction fires this boundary | the rewrite re-bills the prefix anyway |
| A2 | model/provider switch | caches are model-scoped |
| A3 | reasoning-effort switch | message-level break covers folds |
| A4 | cold resume (idle past profile `cold_after`) | TTL expired |
| A5 | context below profile `min_cacheable_tokens` | nothing cached yet |
| A6 | manual `/compact` | user-initiated break; folds ride it |
| B | mid-session idle gap past `cold_after` | TTL expired (inferred) |
| C | configurable token watermark | pressure backstop |
| I | explicit immediate policy | user accepts a warm rewrite at every safe boundary |

`toolResultCompaction.cacheTiming` selects which triggers release pending local
folds:

| Policy | Triggers |
|---|---|
| `breakOnly` | A1-A6 |
| `cacheAware` | A1-A6, B, C |
| `pressureOnly` | A1, C |
| `immediate` | A1-A6 when present, otherwise I |

`cacheAware` is the structured and legacy default. The pressure threshold is
`triggerTokens`, default 64,000; legacy `microcompactionWatermark` supplies it
when no structured block exists.

Provider cache economics live in a provider-neutral `CacheProfile`
(`cold_after`, `probably_cold_after`, `write_premium`, `read_rate`,
`reports_writes`, `min_cacheable_tokens`). The provider→profile table lives in
mimir (`cache_profile(&selection)`); wayland consumes only profile fields and
never sees provider names. Unknown lanes degrade safely: cold triggers off,
break events still valid, watermark unchanged.

Every flush records its trigger class on the persisted `fold` entry and a new
`FoldApplied` observer event; `/context` itemizes the window (system+tools,
raw vs summarized conversation, folded-reclaimed with per-batch tags, pending
mass, headroom). `clear_tool_uses` and local reducers require disjoint eligible
tool sets (ADR-0022 addendum, narrowed 2026-07-09).

Compaction is unchanged: the scheduler re-times fold writes only. Opt-in
stays opt-in; rebuild honors persisted folds regardless of the setting;
originals stay recoverable (ADR-0046).

## Measured economics

All committed in `docs/benchmarks/issue-400-fold-flush-cost.md`:

- Class A arms: marginal modeled write ≤ 0 per trigger (measured −145 on the
  shared seed), steady-state saving 144/turn, CI-asserted.
- Class B live pair (one run per lane, real 390 s idles): Anthropic realized
  write delta −355 tokens (`cache_read = 0` on both runs — TTL truly
  expired); Codex (write-blind lane) post-gap `cached_tokens = 0` on both
  runs, 317-token input saving.
- Wrong cold inference costs one warm flush (4,485 modeled / 2,129 realized)
  — bounded, and strictly less than watermark-only behavior paid on every
  flush.

## Alternatives considered

### Copy Claude Code's hardcoded 60-minute idle constant
- **Pros**: one constant, shipped precedent.
- **Cons**: only correct for Anthropic's 1 h tier; wrong for the 5 m default
  (6 min) and unverifiable for Codex retention.
- **Why not**: deriving thresholds from the provider profile makes the same
  logic correct on every lane simultaneously.

### Flush on realized cache misses (prior-turn usage)
- **Pros**: uses provider-reported truth.
- **Cons**: a miss means the missed request *wrote* a fresh prefix — the
  cache is warm again by the next boundary. Tells you about the past.
- **Why not**: realized usage is Phase 3 calibration input (#395), not a
  trigger.

### Server-side trim while warm (`cache_edits`)
- **Pros**: would avoid the rewrite entirely.
- **Cons**: Anthropic-internal, not public; `clear_tool_uses` invalidates
  caches and drops content Iris still models as present.
- **Why not**: no design may depend on non-public API surface; the public
  edit conflicts with local accounting (ADR-0022 addendum).

## Consequences

### Positive
- Fold flushes ride breaks that happen anyway; the watermark becomes rare
  backstop instead of the only (and worst) path.
- The profile seam prices both lanes correctly (Anthropic-long's 2× write
  premium makes timing twice as important as on the default lane).
- Every reduction is visible and trigger-tagged (`/context`), which powers
  Phase 3 calibration and honest per-trigger claims.

### Negative
- More scheduler state (boundary-scoped break flags, activity clock) to hold
  in tests.
- Class B inference can be wrong; cost is one warm flush, measured and
  bounded.
- The profile table is static documentation-derived data; provider changes
  require a table update (guarded by the mimir profile tests).

### Non-goals (explicit)
- No default-on flip (needs the retention-quality benchmark, not just
  economics).
- No default failure-output folding. Local clearing includes failures only with
  explicit `includeFailures`; native clearing requires the same consent because
  Anthropic exposes no status selector.
- No dependence on `cache_edits`; no server-side `compact`.
- Phase 3 calibration (persisted per-turn `ProviderUsage`, watermark
  retuning, probabilistic Codex threshold, auto-1h retention) deferred to
  #395.

## Open questions
- Does the Codex subscription backend honor `prompt_cache_retention`, and
  what does it default to? Measure via realized `cached_tokens` across a
  controlled idle gap (tracked on #395).
