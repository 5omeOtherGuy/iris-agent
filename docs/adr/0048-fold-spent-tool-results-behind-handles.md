# ADR-0048: Fold spent tool results behind handles (opt-in microcompaction)

**Date**: 2026-07-04
**Status**: accepted; flush *timing* extended by [ADR-0051](0051-cache-aware-fold-flush-scheduling.md) (cache-aware trigger classes; the watermark is now the Class C backstop)
**Deciders**: Iris maintainers, Claude agent session

## Context

Iris reduces tool output at two points today: ADR-0037 filters command output at capture
(50-98% reduction on the measured command classes), and ADR-0011 moves oversized outputs
behind session-scoped handles at ingestion. Both are per-call. Nothing addresses per-session
accumulation: results that were load-bearing for the turn they served and are dead weight
after. The classes that survive filtering and accumulate:

- **Superseded reads.** A `read` result cannot be noise-filtered (the content is the
  payload); an earlier read of a path superseded by a later read or edit of the same path is
  stale by construction.
- **Retired failure output.** ADR-0037's quality guards keep failing-test names, panics, and
  diagnostics verbatim — correct at capture, spent once a later run of the same command
  passes.
- **Accumulated small results.** Many filtered results still sum to a large stale mass over
  a long session.

Full compaction (ADR-0009/0041) is the only relief today, and it is coarser and lossier than
these classes need: a deterministic per-result fold loses nothing that is not recoverable.

One storage constraint shapes the design: the rebuild path rejects overlapping compaction
coverage as invalid session data (`rebuild_with_compactions`, `src/session.rs`), so
per-result folds cannot be expressed as small compaction entries without breaking later
range compaction over the same region.

Tracked in #378. Whether these classes dominate in practice is measured before
implementation (see Decision); reduction claims stay goals until then (ADR-0036 rule 5).

## Decision

Fold spent tool results to deterministic stubs, opt-in, with recovery one step away.

- **Fold = replace content, never remove messages.** A fold replaces a `Tool`-role result's
  content with a deterministic stub; the tool call stays intact. Provider pair invariants
  (a result for every call) hold trivially; `plan_compaction`'s pair-awareness is untouched.
- **A durable, id-keyed `fold` entry kind.** Not a compaction entry: folds target single
  message ids, and the compaction overlap-rejection rule must keep holding for range
  coverage. Precedence at rebuild: compactions apply first; a fold whose target id lies in a
  covered range is a no-op. Folds apply in memory and durably in the same step, so live and
  resumed context agree (the ADR-0009 invariant).
- **Recovery per result class.** Reproducible results (read/ls/grep/find): the stub names
  the path or command — the workspace is the recovery path, no new machinery.
  Non-reproducible results (bash output): the folded content registers in the
  `ToolOutputStore` at fold time and the stub carries the handle id; `read_output` pages it
  back (ADR-0011).
- **Fold policy.** Batch folds at a micro-watermark below the compaction budget, so one
  prefix cache break amortizes many folds; never fold inside the retained tail; never fold
  an error-classified result (ADR-0040) that no later success of the same command has
  superseded.

**V1 scope (re-scoped on the committed M2 benchmark).** The measurement gate
(`docs/benchmarks/issue-378-residual-tool-mass.md`) found superseded reads + retired-failure
output do NOT dominate residual tool mass (~19.5% overall, ~32.3% in long sessions), and
within that foldable slice superseded reads are effectively the entire signal (~18%); retired
failure output is negligible (~1.5%, an identical-rerun upper bound, since bash exit status is
not persisted). **V1 therefore folds superseded reads only (latest-read-wins,
workspace-recoverable).** Retired-failure-output folding and bash-output-handle folding are
**deferred** on that evidence — not architected out: the fold engine is a set of pluggable
policies (`src/wayland/fold.rs`), so each deferred class plugs in later as an additional
policy arm and stub variant without reworking the pass. The stub format stays extensible for
the bash `ToolOutputStore` handle it will carry.
- **Opt-in, default off, surfaced in `/settings`.** A `microcompaction` config field
  (camelCase key `microcompaction`), project-tunable as a cost/quality knob like
  `compactionSummarizer` — it cannot redirect requests, and everything folded stays
  recoverable. The `/settings` menu gains a toggle; changes take effect at the next turn
  boundary. The setting gates fold *writing* only: rebuild always honors persisted fold
  entries, so a transcript folded under one setting reads identically under another.
- **Measurement gates implementation.** Before the fold engine is built, a read-only report
  over real session transcripts (per-entry `tokenEstimate` is persisted) establishes the
  residual tool-result mass by tool class and age, committed under `docs/benchmarks/`. If
  superseded reads do not dominate, the first slice is re-scoped. Folding then becomes an
  arm in the ADR-0045 A/B, with the needle contract: a needle survives in rebuilt context or
  behind a named handle that survives verbatim.

## Alternatives Considered

### Alternative 1: Express folds as single-message compaction entries
- **Pros**: No new entry kind; reuses coverage semantics.
- **Cons**: Overlap rejection makes a later range compaction over any folded id invalid;
  range planning would fragment around every fold or fail the read.
- **Why not**: A separate kind with a compaction-wins precedence rule keeps both layers
  simple and the ADR-0009 invariant intact.

### Alternative 2: Fold at ingestion instead (tighter filters, lower handle threshold)
- **Pros**: No new persistence; already-proven seams (ADR-0011/0037).
- **Cons**: "Spent" is only knowable in hindsight — a result cannot be cut below
  current-turn needs at capture time; that is exactly the line ADR-0037's guards enforce.
- **Why not**: Ingestion-time and residency-time reduction are different stages; this ADR is
  the second stage, not a retuning of the first.

### Alternative 3: Fold in memory only, no durable entry
- **Pros**: No format addition.
- **Cons**: Live and resumed context diverge — resume would replay unfolded results the live
  session no longer carries.
- **Why not**: Violates the live/resumed agreement invariant that compaction already keeps.

### Alternative 4: Always-on folding
- **Pros**: Benefit without a knob; one behavior to test.
- **Cons**: Folding trades in-context detail for recoverable detail; until the ADR-0045
  benchmark proves task success holds, forcing that trade on every session is premature.
- **Why not**: Opt-in with a measured gate; the default can flip when the evidence exists.

## Consequences

### Positive
- Retires stale tool-result mass that per-call filtering cannot touch, at deterministic
  cost: no provider round-trip, no summary-quality risk.
- Completes the reduction ladder — filter at capture (ADR-0037), offload oversized
  (ADR-0011), fold when spent (this ADR), summarize under pressure (ADR-0009/0041/0043),
  recall on demand (ADR-0046) — each rung measured and reversible.
- Reuses the handle store, `read_output`, error classes (ADR-0040), and structured result
  paths (ADR-0021); no new tool surface.

### Negative
- A new entry kind plus a precedence rule to hold in rebuild, tests, and future tooling.
- Each fold batch is a one-time prefix cache break (bounded by batching at the
  micro-watermark). Measured (`docs/benchmarks/issue-400-fold-flush-cost.md`): on a warm
  cache a fold-only flush re-bills everything below the fold point (realized 2129
  provider-reported write tokens on the live seed) against a per-turn saving of the folded
  body -- break-even tens-to-hundreds of turns -- while the same fold at a compaction
  boundary adds no marginal write and a cold cache makes the flush free. Cache-aware flush
  timing that exploits this is #400.
- Off by default, the benefit reaches only users who opt in until the benchmark justifies
  flipping the default.

### Risks
- Double reduction defeats the ADR-0037 guards: folding a filtered result discards the
  failure detail filtering deliberately kept. Mitigate: the fold policy honors the same
  guard classes (unresolved failures never fold), and ADR-0045 needles assert failure detail
  survives or sits behind a named handle.
- A stub could mislead the model about what it has already seen. Mitigate: stubs are
  explicit about what was folded and how to recover it, and the system-prompt fragment
  (ADR-0046) covers folds alongside compaction markers.
- Fold-then-compact interleavings could corrupt rebuild. Mitigate: precedence is tested in
  both orders (fold then compact over the range; compact then attempted fold inside it).
