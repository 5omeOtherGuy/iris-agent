# ADR-0062: Share Codex prompt heads only on isolated WebSocket sessions

**Date**: 2026-07-13
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

ADR-0022 enables provider-native prompt caching, but Codex previously used the
Iris session id as `prompt_cache_key`. A new session therefore missed an
otherwise identical system-prompt prefix. Replacing it with one shared key for
all requests is unsafe: concurrent conversations can diverge after the shared
head, and the Codex SSE transport has no connection-local continuation state to
keep those warm branches independent.

Anthropic has no Iris-supplied routing key. Its exact-block `cache_control`
markers already let compatible requests reuse an identical system prefix while
conversation histories remain separate.

## Decision

Compatible Codex WebSocket sessions share a deterministic request-head routing
key. Deeper conversation state remains local to each provider instance.

- Mimir derives the shared key from a version tag, the workspace path, and the
  assembled system prompt with SHA-256. The `iris:v1:` key is opaque and no
  longer than OpenAI's 64-character limit.
- The workspace and assembled prompt are cache-compatibility boundaries. A
  different workspace, project instruction set, or system prompt produces a
  different key. Provider credentials remain the account boundary.
- A Codex WebSocket's first full request may reuse an exact matching head from
  another compatible session. Later turns use that session's socket and
  `previous_response_id` continuation, so concurrent warm branches do not share
  mutable continuation state.
- Explicit SSE, automatic WebSocket-to-SSE fallback, native compaction, and the
  OpenAI platform adapter retain session-scoped keys. These paths have no
  equivalent connection-local branch boundary.
- Anthropic behavior is unchanged. Exact `cache_control` blocks provide
  cross-session reuse without introducing a shared Iris routing key.
- Nexus remains provider-neutral. Session ids still own persistence, resume,
  task linkage, and background-work validation; Mimir alone owns cache routing.

Live probes with two divergent sessions validated the boundary on 2026-07-13.
Codex WebSocket session B read the shared head, then both concurrent follow-up
turns read their own deeper prefixes. Anthropic showed the same shared-head and
independent-branch pattern. The Codex SSE probe kept response branches separate;
deterministic request tests verify that SSE receives distinct session keys.
Provider-reported zero cache reads remain inconclusive and are not treated as a
correctness failure.

## Alternatives Considered

### Use one stable key on every Codex transport

- **Pros**: Maximizes opportunities for cross-session hits.
- **Cons**: SSE conversations would route divergent histories through one cache
  bucket without connection-local continuation.
- **Why not**: A live SSE probe did not preserve both warm branches reliably.
  Isolation takes priority over speculative cache reuse.

### Switch from a shared key to a session key after the first request

- **Pros**: Makes the first-turn boundary explicit in the key itself.
- **Cons**: Changes routing while the provider is establishing continuation and
  discards the WebSocket's native branch mechanism.
- **Why not**: Separate sockets plus `previous_response_id` already provide the
  validated session boundary. SSE, where that mechanism is absent, uses session
  keys from the start.

### Use the Iris session id everywhere

- **Pros**: Preserves complete routing isolation.
- **Cons**: Every fresh Codex session pays for the same large workspace/system
  prefix again.
- **Why not**: The WebSocket transport provides a narrower, tested sharing seam.

### Put a cache identity in Nexus

- **Pros**: Gives every provider one apparent cache abstraction.
- **Cons**: Codex keys, WebSocket continuation, SSE fallback, and Anthropic cache
  blocks have different semantics.
- **Why not**: Cache routing is provider policy and belongs in Mimir.

## Consequences

### Positive

- Fresh compatible Codex WebSocket sessions can reuse a warm system-prompt head.
- Concurrent sessions retain independent warm conversation branches.
- Workspace, prompt, account, and session boundaries are explicit.
- Resume and task identity remain independent of provider cache identity.

### Negative

- Codex SSE and OpenAI platform sessions do not gain cross-session head reuse.
- A system-prompt or workspace change intentionally creates a cold cache key.
- Cache benefit still depends on provider minimum sizes, timing, retention, and
  exact-prefix matching.

### Risks

- OpenAI may change WebSocket continuation or cache-key semantics. Keep the
  ignored live probes and deterministic SSE-routing tests as regression gates.
- A reconnect can force a full request. The adapter must preserve the
  session-scoped SSE key whenever it leaves the WebSocket continuation path.
- Hashing prevents prompt text from appearing in the routing key, but the key is
  not an authorization boundary; provider credentials remain mandatory.
