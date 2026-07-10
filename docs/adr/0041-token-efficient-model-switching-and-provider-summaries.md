# ADR-0041: Token-efficient model switching and provider-backed compaction summaries

**Date**: 2026-07-02
**Status**: accepted
**Deciders**: Iris maintainers, Claude agent session

## Context

Iris already switches provider/model/reasoning at safe turn boundaries
(ADR-0017), persists reasoning continuity per row (ADR-0016), compacts context
through a durable `compaction` entry (ADR-0009), and enables short provider
cache hints by default (ADR-0022). What none of those slices addressed is the
token cost of the switch itself:

- **Foreign reasoning replay.** After a model change, ADR-0016 downgraded a
  foreign-origin visible reasoning row to a plain `text` block on the Anthropic
  lane, and the generic OpenAI-compatible lane replayed every reasoning row as
  assistant content. Both choices re-bill the old model's chain-of-thought as
  input tokens on every subsequent request until compaction happens to cover
  it. Reasoning text is redundant with the visible answer and tool calls it
  produced, so this is pure overhead: the Codex Responses adapter already drops
  foreign reasoning, and Antigravity skips reasoning rows entirely.
- **Uncached re-ingest.** Provider prompt caches are model-keyed. A same
  provider model change (Opus -> Sonnet) or a cross-provider change (Opus ->
  GPT 5.5) makes the next request re-read the whole carried context at the
  uncached input rate and re-fills the new model's context window with old
  turns. Iris switched silently, so the user had no signal that a large context
  was about to be re-read, and no tool to shrink it first: compaction only
  triggered on the token budget, and the only summarizer was the deterministic
  bounded-excerpt stand-in (`wayland::summarize`) that issue #55 left as the
  explicit swap point.
- **Reasoning-only changes are already cheap.** Changing only the effort level
  keeps the message prefix byte-identical; nothing needs to change there except
  not scaring the user into thinking a switch costs something.

pi-mono (the contract reference) resolves the same tension with an LLM-written
session summary the user can invoke and that compaction uses; this ADR adopts
that shape on top of Iris's existing durable compaction entry.

## Decision

1. **Foreign-origin reasoning is never replayed to a provider.** The Anthropic
   adapter drops a foreign-origin visible reasoning block instead of
   downgrading it to `text` (amends ADR-0016's wire rule; the row itself stays
   persisted and display-visible). The OpenAI-compatible chat adapter stops
   replaying reasoning rows as assistant content. This makes "reasoning is
   same-origin-replayed or dropped" the uniform rule across all four adapters.

2. **A provider-backed summarizer becomes the default compaction summarizer.**
   The harness summarizer seam gains a `SummarizerKind`: `provider` (default)
   asks the active provider to write a structured handoff summary of the
   covered range (goal, state, decisions, touched files, next steps), reusing
   the live context prefix and tool declarations so the request rides the warm
   provider cache; `excerpts` keeps the deterministic stand-in. Any provider
   failure, empty answer, or a summary that fails to shrink the covered range
   falls back to the deterministic excerpts, and cancellation skips compaction
   for the turn. The `compaction` entry format (ADR-0009) is unchanged; only
   the summary text source differs. The setting (`compactionSummarizer`) is
   project-tunable like `contextTokenBudget` because it is a cost/quality knob,
   not a routing or retention control.

3. **`/compact` becomes a first-class command.** It compacts on demand at the
   inter-turn boundary, keeping a small recent tail (pair- and turn-aware, same
   planner as auto-compaction) and replacing the older range with the summary.
   It works in the TUI (driven like a turn: spinner, Ctrl-C cancel) and the
   text path.

4. **Switches report their context cost and offer summarization.** Every
   switch classifies as reasoning-only, same-provider model change, or
   provider change. A reasoning-only switch stays silent (context and prefix
   carry over unchanged). A model or provider change with a large carried
   context (over a quarter of the context budget) appends an advisory to the
   switch confirmation: the estimated tokens the new model will re-read
   uncached, and `/compact` as the way to shrink first. The switch itself is
   never blocked or made conditional.

## Alternatives Considered

### Keep downgrading foreign visible reasoning to text
- **Pros**: The new model sees the old model's reasoning; no ADR-0016 change.
- **Cons**: Pays the old chain-of-thought as input on every later request;
  reasoning is redundant with the answer/tool calls it produced; no other
  adapter lane does this.
- **Why not**: Contradicts the token-efficiency thesis for marginal value; a
  user who wants the old reasoning visible still has it in the transcript.

### Auto-compact on switch instead of advising
- **Pros**: Maximum seamlessness; no user action needed.
- **Cons**: Destroys detail without consent at an unexpected moment, adds a
  provider round-trip to every switch, and surprises users who switch back.
- **Why not**: A durable, lossy rewrite should stay user-invoked; the advisory
  plus a one-word command keeps the flow one step while leaving history intact
  by default.

### Summarize with the outgoing provider automatically before the switch lands
- **Pros**: The summarization request rides the old model's warm cache.
- **Cons**: Requires an async provider call inside the currently synchronous
  switch path, couples switching to compaction, and still needs consent.
- **Why not**: `/compact` before switching gives the same cache benefit
  explicitly; the advisory documents the cost either way. Revisit if a
  mode-profile system later wants policy-driven switch compaction.

### A new summarizer trait/provider registry
- **Pros**: Pluggable local/remote summarizers.
- **Cons**: More surface than the one real implementation needs; ADR-0009
  already isolated the summary text source.
- **Why not**: The seam is a function choice inside the harness; a trait can be
  extracted when a second real summarizer exists.

## Consequences

### Positive
- After any switch, requests stop carrying the old model's reasoning text, so
  the per-request cost drops immediately and permanently.
- Users see what a model/provider switch will re-read before the next turn
  pays for it, and can shrink it to a provider-quality summary with one
  command.
- Auto-compaction produces provider-quality summaries by default, with the
  deterministic excerpts as an always-available floor (and still the only
  summarizer for in-memory/no-log sessions, which never compact).
- The compaction storage/rebuild contract, session format, and resume path are
  untouched.

### Negative
- Provider-backed summaries add one model round-trip (and its output cost) to
  each compaction; the fallback ladder adds a failure mode to reason about.
- The advisory threshold (budget/4, or 32k without a budget) is a heuristic;
  small-but-expensive contexts stay silent and large-but-cheap ones advise.
- Dropping foreign reasoning means a switched-to model genuinely sees less
  than before on the Anthropic lane (the visible reasoning text).

### Risks
- A provider summary could omit load-bearing detail; mitigate with the
  structured handoff prompt, the shrink guard, and the durable original turns
  (compaction is a read-time view, never history destruction).
- The summarization request happens mid `submit_turn` for auto-compaction; a
  slow provider delays the turn start. Mitigate: the request is cancellable
  (Ctrl-C skips compaction, then cancels the turn normally) and budget-gated
  so it fires rarely.
- Foreign-reasoning dropping changes provider-visible bytes for existing
  transcripts that already carry foreign rows; the prefix fingerprint treats
  this as the prefix change it is (a one-time cache break at upgrade).

## Addendum (2026-07-10, auto-compaction worker v2)

`compactionSummarizer` now selects who answers; `compaction.worker.input`
selects the request shape. `transcript` is the default: covered messages are
sent verbatim, followed by the summary instruction. `investigator` retains the
read-only workspace-probing worker. A transcript request that reports context
overflow drops one oldest covered message and retries until it succeeds or the
slice is empty.

`compaction.worker.model` is global-only and resolves a qualified
`provider/model` through Mimir. Unset workers follow the active selection.
Both routes receive the same cancellation token. `compaction.instructions` and
bounded `/compact <focus>` text are appended to the instruction and recorded on
the durable compaction entry.

Manual compaction uses the one-slot background pipeline. When a job already
exists, `/compact` awaits it instead of cancelling and restarting it. Parent
code remains the only apply point.
