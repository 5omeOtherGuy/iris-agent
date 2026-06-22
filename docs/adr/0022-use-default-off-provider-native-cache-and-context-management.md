# ADR-0022: Use default-off provider-native cache and context-management controls

**Date**: 2026-06-22
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Milestone 2's token-efficiency thesis needs provider-native cache and context
signals, but those signals affect privacy, cost, provider request shape, and
resume correctness. OpenAI Codex Responses exposes prompt-cache key and retention
fields plus usage/cache accounting. Anthropic exposes public `cache_control`
markers and `context_management.edits`, including clear-tool-use, clear-thinking,
and compact edits. Iris already has a provider-neutral Nexus core, global/project
settings split, JSONL session persistence, turn-boundary compaction, provider
usage events, and flattened transcripts. The question is how to integrate these
provider-native controls without silently changing request behavior or letting a
repo-local config opt users into longer provider-side retention.

## Decision

Iris supports provider-native prompt cache and context-management features only
through explicit, default-off, global/user settings.

- `promptCacheRetention` is global-only and parses as `none`, `short`, or
  `long`; absent means `none`. OpenAI receives `prompt_cache_key` when caching is
  enabled and `prompt_cache_retention: "24h"` only for `long`. Anthropic receives
  `cache_control: { type: "ephemeral" }` for `short` and adds `ttl: "1h"` for
  `long`.
- `anthropicContextManagement` is global-only. Iris supports public
  clear-tool-use and clear-thinking edits, but rejects Anthropic compact until a
  provider `compaction` response block can be represented, persisted, and replayed
  safely in the session store.
- Mimir owns all provider-specific request fields and diagnostics. Nexus carries
  only provider-neutral completion usage/cache metadata and typed UI events.
- Cache diagnostics distinguish request-side opt-in from provider-reported cache
  hits. A zero cache-read count is not treated as a cache break. Iris warns only
  when it can prove the stable prompt prefix changed between cache-enabled turns
  (instructions/tools changed, history shrank, or an earlier message diverged).
- Project settings cannot set `promptCacheRetention` or
  `anthropicContextManagement`; cloned repositories may tune model, reasoning,
  and context-budget behavior, but not provider routing, bearer-token endpoints,
  provider-side cache retention, or server-side context edits.

## Alternatives Considered

### Enable provider cache hints automatically

- **Pros**: More users may see cache hits without configuration.
- **Cons**: Changes request shape, retention behavior, and possibly cost/privacy
  without opt-in; cache behavior differs by provider.
- **Why not**: Token efficiency must not come from hidden provider-side retention
  or billing changes. Default-off preserves byte-identical requests unless the
  user opts in.

### Let project config choose cache/context-management settings

- **Pros**: Repositories could standardize their desired optimization policy.
- **Cons**: A cloned repo could silently increase provider-side prompt retention,
  trigger cache writes, or enable server-side context edits for user content.
- **Why not**: These settings affect privacy, cost, and request semantics, so
  they follow provider/base-url/scoped-model controls as global-only.

### Adopt Anthropic compact immediately

- **Pros**: Stronger provider-native context reduction.
- **Cons**: Compact responses introduce provider-authored compaction blocks that
  must survive transcript persistence, resume, and future request replay. Dropping
  or misordering them risks invalid or lossy conversations.
- **Why not**: Iris already has a durable local compaction foundation. Provider
  compact should wait until the session format can represent and replay compact
  blocks deliberately.

### Build a provider-agnostic cache abstraction in Nexus

- **Pros**: One public concept for every model/provider.
- **Cons**: Provider cache mechanisms are not semantically identical; lifting them
  into Nexus would leak provider policy upward or hide important differences.
- **Why not**: Nexus should remain provider-neutral. Mimir adapts provider-native
  knobs while Nexus only observes normalized usage/cache metadata.

## Consequences

### Positive

- Existing users keep the same request behavior until they opt in.
- Cache and context controls sit at the provider boundary that understands their
  wire formats and caveats.
- Repo-local config cannot silently expand prompt retention or enable server-side
  edits.
- Diagnostics avoid false cache-miss blame and report only proven prefix breaks.

### Negative

- Users must opt in manually before seeing provider cache benefits.
- `promptCacheRetention` is a coarse cross-provider knob; exact cache semantics
  still differ between OpenAI and Anthropic.
- Anthropic compact remains unavailable until the transcript/replay contract is
  extended.

### Risks

- Provider APIs may change cache or context-management field names/semantics;
  mitigate with focused provider request-construction tests.
- A future mode/subagent system may need per-worker cache policies; keep the same
  default-off/global-owner invariant and pass resolved policy into each worker
  rather than reading repo config directly.
- Provider usage metadata may be incomplete or absent; UI and benchmarks must
  treat missing usage as unknown, not zero savings.
