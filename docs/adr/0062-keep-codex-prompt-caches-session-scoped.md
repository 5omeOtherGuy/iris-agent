# ADR-0062: Keep Codex prompt caches session-scoped

**Date**: 2026-07-14
**Status**: accepted

## Context

Iris considered sharing a workspace-and-system-prompt cache key across compatible Codex sessions. The design kept WebSocket `session-id` and `x-client-request-id` session-scoped, used the shared key only for the first WebSocket request, then returned retries, reconnects, later turns, SSE, and compaction to the session key. Live verification was required because response continuation and prompt-cache routing are provider behavior, not an Iris guarantee.

## Decision

Keep Codex prompt-cache routing session-scoped. Do not expose a cross-session cache setting until an official provider contract and a repeatable live probe demonstrate shared-head reuse without sharing transport identity. Keep Anthropic's existing exact-prefix `cache_control` behavior unchanged.

## Evidence

The Anthropic probe confirmed the desired behavior without Iris-side routing: the second session read 12,639 cached tokens from the shared head, then both branches independently read more than 17,650 cached tokens.

The explicit Codex WebSocket probe confirmed that both sessions remained on WebSocket and independently warmed their branches. Reusing one `prompt_cache_key` across distinct transport sessions did not share the head: the second session reported zero cached tokens. Follow-up turns reported 16,640 cached tokens per session. The safe design therefore preserved isolation but provided no cross-session benefit.

## Alternatives considered

### Share a cache key while keeping transport identities distinct

- Preserves Iris session isolation.
- A one-shot key can limit shared routing to the compatible request head.
- Rejected because repeated live WebSocket probes produced no cross-session cache read.

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
- Future work needs both documented provider semantics and explicit transport-observed live evidence before changing this decision.
