# ADR-0043: Provider waits are visible and bounded by provider-event idleness

**Date**: 2026-07-05
**Status**: accepted
**Deciders**: Iris maintainers

## Context

A running Iris turn can look stuck when the working indicator spins but the
visible transcript does not change. There are two different states behind that
same symptom:

- the model/provider is still working before it emits user-visible text or tool
  calls; and
- the pager is not currently following the live tail, so new rows may be below
  the viewport.

The approval and tool paths are already evented: approval panels, tool starts,
tool output, tool results, denials, and errors produce UI events. A silent wait
with an established provider HTTPS connection is therefore not evidence of a
blocked local read/write or auto-approval decision. It is usually a provider
wait, including long non-streamed reasoning or buffered tool-call input.

The transport also had only a generous whole-request timeout. That protects
against requests that never finish, but it lets a provider accept an SSE request
and then produce no events for a long time while the UI shows only a spinner.
Cancelling can stop the consumer side, but a blocking provider read may not wake
until bytes arrive or the HTTP client timeout fires.

## Decision

Make provider waits explicit and bound provider-event idleness:

1. Add a provider-neutral `ProviderEvent::Activity` for streamed provider
   progress that carries no user-visible text yet, such as reasoning deltas,
   tool-call input deltas, pings, or provider frames buffered until the terminal
   `Completed` turn.
2. Extend the shared Mimir `TurnSink` with `on_activity()`. Provider adapters
   call it for every parsed SSE event before feeding the event to their parser;
   text deltas still use `on_text_delta()` and completed turns still end with
   `ProviderEvent::Completed`.
3. Wrap the provider event channel in `spawn_stream()` with a 90-second
   provider-event idle timeout. The timeout is reset by text, completion, and
   activity events. If no provider event arrives within that window, the stream
   cancels the turn token and yields an error instead of leaving the UI spinning
   until the whole-request backstop.
4. Surface provider waits in the TUI working indicator with a compact `model`
   label from `ProviderTurnStarted` until assistant text, tool lifecycle,
   completion, cancellation, error, denial, or result clears it.

The idle guard is based on provider events, not text. Long legitimate reasoning
or tool-call-input streams remain live as long as the provider continues sending
SSE frames.

## Alternatives Considered

### Alternative 1: Keep only the whole-request timeout
- **Pros**: No protocol changes; avoids any risk of timing out long model work.
- **Cons**: A provider can leave the user with only a spinner for many minutes;
  the UI cannot distinguish provider wait from hidden approval/tool blockage.
- **Why not**: The bug is an observability and boundedness failure during a live
  turn. The whole-request timeout is too coarse to explain or recover from
  provider idleness.

### Alternative 2: Time out only on missing text deltas
- **Pros**: Simple: reset the timer only when the user-visible transcript moves.
- **Cons**: False-fails providers that stream reasoning, pings, or tool-call
  arguments while buffering final text/tool calls until completion.
- **Why not**: Iris supports providers where non-text frames are real progress.
  Idleness must mean no provider events, not no visible text.

### Alternative 3: Render every provider activity event
- **Pros**: Maximum transparency; the user sees that bytes are arriving.
- **Cons**: Noisy, provider-shaped UI; risks exposing low-value implementation
  details and bloating the transcript with frames that are not model messages.
- **Why not**: The needed signal is state, not content. A compact working-label
  and transport idle guard are enough; the transcript stays semantic.

## Consequences

### Positive

- A provider wait is visibly different from an approval prompt or local tool
  execution.
- Silent provider stalls fail after a bounded idle window instead of spinning
  until the whole-request timeout.
- Long non-text provider streams do not false-timeout because activity events
  reset the idle guard.
- The Nexus/Mimir tier split stays intact: Mimir adapters report provider
  activity; Nexus consumes provider-neutral events; Iris renders the status.

### Negative

- Adds one provider-neutral event variant that is intentionally ignored by the
  transcript.
- Every streaming provider adapter must remember to call `on_activity()` for
  parsed SSE frames.
- A provider that legitimately sends no event for more than 90 seconds during a
  turn now fails rather than waiting for the 30-minute total backstop.

### Risks

- **False timeout for quiet providers**: mitigated by using provider-event
  idleness rather than text idleness, and by keeping the window at 90 seconds.
- **Activity-event noise**: mitigated by not rendering activity events as rows;
  they only reset transport idleness and preserve the existing semantic
  transcript.
- **Adapter drift**: tests and code review should check that new streaming
  adapters call `on_activity()` around their SSE parser loop.
