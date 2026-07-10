# ADR-0055: Govern context between provider round trips

**Date**: 2026-07-10
**Status**: accepted
**Deciders**: Iris maintainers

## Context

The model-aware ladder in ADR-0054 ran only before and after a user turn. A
long tool loop could cross `start` or `hard` with no relief, and a background
summary that finished during the loop could not apply until a later turn.

Incremental persistence now gives every completed round trip durable message
ids before the next provider request. The runtime needs one provider-neutral
boundary seam that can use those ids without moving sessions, budgets, or
summarizers into Nexus.

## Decision

Nexus defines `ContextGovernor`. It receives a `BoundaryContext` containing the
pair-closed message list, the latest provider-usage anchor, the completed round
trip number, and whether the turn will continue. It returns either `Proceed` or
one whole-message-list `Replace` directive.

The loop consults the governor only when another provider request will follow:

1. record every tool result or completed tool-less response;
2. append the completed group through the harness observer;
3. consult the governor and apply any replacement atomically;
4. inject queued steering or follow-up text;
5. start the next provider request.

Cancellation races the governor future. A governor error emits a notice and
never fails the user's turn. Nexus applies the returned list but knows nothing
about compaction policy, storage, worker models, or provider selection.

Wayland's `TurnContextController` implements the persistence and governor
contracts over one `CompactionEngine`. At a governed boundary the engine:

- drains a ready worker without waiting;
- measures the current context and emits pressure crossings;
- starts one worker at `start` and immediately returns;
- waits only at `hard`, bounded by `hardWaitMs`, then uses deterministic relief;
- durably validates and appends through the same parent-owned apply function
  used at turn edges;
- returns the rebuilt context for Nexus to install.

The same governor also exposes one provider-neutral overflow-recovery entry
point. Nexus invokes it only for a typed context-window failure before any
assistant output is visible. Mimir owns adapter-specific HTTP classification;
provider names and wire shapes do not cross the Tier-1 boundary. Wayland
recovers deterministically in this order: flush eligible folds, apply excerpts
with the configured recent tail, then retry excerpts with a 1,000-token tail if
the context is still hard. The parent persists every rewrite before Nexus
installs it and resends the request.

Nexus permits one recovery and resend per provider round trip. A second
overflow, an overflow after visible output, or a recovery that cannot produce
a pair-safe rewrite returns an honest error with the measured context/window
and the `/compact <focus>`, `/new`, and model-switch options. Successful
provider completion resets the guard for a later tool round trip. Automatic
model-backed compaction is capped at two applies per user turn; later pressure
uses deterministic excerpts.

An active background job freezes folds whose durable ids fall inside its
covered range. Folds outside the range can flush. Releasing or applying the job
removes the freeze. This prevents the worker snapshot and fold scheduler from
rewriting different versions of the same original range.

## Alternatives Considered

### Put budget and compaction logic in Nexus

- **Why not**: It reverses the tier dependency and gives the core loop session,
  storage, and provider-policy knowledge.

### Expose several mutation callbacks

- **Why not**: A whole-list replacement matches the existing context swap and
  keeps one atomic mutation point.

### Poll only at user-turn edges

- **Why not**: It cannot protect a single long tool loop or land ready work
  while that loop is still active.

### Wait for every background worker

- **Why not**: `start` must remain non-blocking. Only the hard tier owns a
  bounded wait because the next provider request may otherwise overflow.

## Consequences

- Ready summaries can apply before the next provider request in the same turn.
- Steering remains verbatim and post-summary because it is injected after the
  governor returns.
- Completed tool-call/result groups are never split by a boundary or coverage
  plan.
- Non-hard compaction work is measurable as event-to-next-request latency; the
  live protocol requires it to stay below 200 ms per session.
- A provider context overflow can rewrite and resend once without leaking
  provider-specific policy into Nexus or duplicating visible assistant output.
- The job slot remains process-local and at most one worker runs per session.
- Provider-native compaction remains a later slice.
