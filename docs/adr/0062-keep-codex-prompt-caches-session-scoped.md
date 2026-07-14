# ADR-0062: Keep Codex prompt caches session-scoped

**Date**: 2026-07-14
**Status**: accepted

## Context

Iris considered sharing a workspace-and-system-prompt cache key across compatible Codex sessions. The design kept WebSocket `session-id` and `x-client-request-id` session-scoped, used the shared key only for the first WebSocket request, then returned retries, reconnects, later turns, SSE, and compaction to the session key. Live verification was required because response continuation and prompt-cache routing are provider behavior, not an Iris guarantee.

## Decision

Keep Codex prompt-cache routing session-scoped. Do not expose a cross-session cache setting until an official provider contract and a repeatable live probe demonstrate shared-head reuse without sharing transport identity. Per-session startup prewarming is compatible with this decision because it keeps cache and transport identity private to one session; track it separately in [#621](https://github.com/5omeOtherGuy/iris-agent/issues/621). Keep Anthropic's existing exact-prefix `cache_control` behavior unchanged.

## Evidence

The Anthropic probe confirmed the desired behavior without Iris-side routing: the second session read 12,639 cached tokens from the shared head, then both branches independently read more than 17,650 cached tokens.

The explicit Codex WebSocket probe confirmed that both sessions remained on WebSocket and independently warmed their branches. Reusing one `prompt_cache_key` across distinct transport sessions did not share the head: the second session reported zero cached tokens. Follow-up turns reported 16,640 cached tokens per session. The safe design therefore preserved isolation but provided no cross-session benefit.

The first-party Codex client at `271136e` follows the same ordinary-session boundary: its default [`prompt_cache_key` is the thread ID](https://github.com/openai/codex/blob/271136e00c70965a24eca9225b7efdf36d8b515c/codex-rs/core/src/client.rs#L469-L473), while [WebSocket headers retain session and thread identity](https://github.com/openai/codex/blob/271136e00c70965a24eca9225b7efdf36d8b515c/codex-rs/core/src/client.rs#L1072-L1089). It separately performs a [session-local startup prewarm](https://github.com/openai/codex/blob/271136e00c70965a24eca9225b7efdf36d8b515c/codex-rs/core/src/session_startup_prewarm.rs#L241-L324): send the stable request head with `generate=false`, then continue the first turn on that connection with `previous_response_id`. This is evidence for reducing first-turn latency without cross-session routing, not a provider contract for sharing ordinary sessions. Codex only [overrides the key for the controlled Guardian reviewer](https://github.com/openai/codex/blob/271136e00c70965a24eca9225b7efdf36d8b515c/codex-rs/core/src/guardian/review_session.rs#L205-L216), scoped to one parent thread.

## Alternatives considered

### Share a cache key while keeping transport identities distinct

- Preserves Iris session isolation.
- A one-shot key can limit shared routing to the compatible request head.
- Rejected because repeated live WebSocket probes produced no cross-session cache read.

### Prewarm each Codex session independently

- Opens the session's WebSocket and processes its stable request head before the first user turn.
- Preserves session-scoped cache routing, transport identity, continuation, and fallback.
- Compatible with this decision; implementation remains gated on Iris proving WebSocket v2 request compatibility and live benefit in [#621](https://github.com/5omeOtherGuy/iris-agent/issues/621).

### Share the WebSocket transport identity

- Might place both sessions in the same provider routing scope.
- Rejected because `session-id` and `x-client-request-id` identify a Codex thread/transport session. Sharing them would merge identities and make concurrent continuation unsafe.

### Share a key on SSE or every turn

- Requires less adapter state.
- Rejected because divergent full histories would contend in one routing bucket, retries would keep using shared identity, and [OpenAI's prompt-caching guidance](https://developers.openai.com/api/docs/guides/prompt-caching) recommends keeping traffic below about 15 requests per minute per cache key.

## Consequences

- Concurrent Codex sessions retain independent transport, continuation, retry, and prompt-cache state.
- Fresh Codex sessions do not reuse one another's system-prompt cache.
- Anthropic sessions may reuse exact cache-controlled prefixes under the provider's account/workspace boundaries.
- Per-session Codex startup prewarming may reduce first-turn latency without weakening this isolation; [#621](https://github.com/5omeOtherGuy/iris-agent/issues/621) owns verification and implementation.
- Future cross-session work needs both documented provider semantics and explicit transport-observed live evidence before changing this decision.
