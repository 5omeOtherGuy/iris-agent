# ADR-0022: Use default-short provider-native cache and default-off context-management controls

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

Iris enables short-lived provider-native prompt-cache hints by default and keeps
provider-native context-management features explicit, default-off, and
global/user-only.

- `promptCacheRetention` is global-only and parses as `none`, `short`, or
  `long`; absent means `short`. OpenAI receives `prompt_cache_key` when caching is
  enabled and `prompt_cache_retention: "24h"` only for `long`. Anthropic receives
  `cache_control: { type: "ephemeral" }` for `short` and adds `ttl: "1h"` for
  `long`.
- `anthropicContextManagement` is global-only. Iris supports public
  clear-tool-use and clear-thinking edits, but rejects Anthropic compact until a
  provider `compaction` response block can be represented, persisted, and replayed
  safely in the session store.
- Mimir owns all provider-specific request fields and diagnostics. Nexus carries
  only provider-neutral completion usage/cache metadata and typed UI events.
- Cache diagnostics distinguish the request-side cache setting from provider-reported cache
  hits. A zero cache-read count is not treated as a cache break. Iris warns only
  when it can prove the stable prompt prefix changed between cache-enabled turns
  (instructions/tools changed, history shrank, or an earlier message diverged).
- Project settings cannot set `promptCacheRetention` or
  `anthropicContextManagement`; cloned repositories may tune model, reasoning,
  and context-budget behavior, but not provider routing, bearer-token endpoints,
  provider-side cache retention, or server-side context edits.

## Alternatives Considered

### Keep provider cache hints default-off

- **Pros**: Preserves byte-identical request behavior unless the user opts in.
- **Cons**: Repeated tool loops can replay large stable prefixes uncached and
  consume subscription quota far faster than comparable cached harnesses.
- **Why not**: Short-lived cache hints are the safer default for Iris's coding
  workload. Users who need byte-identical no-cache requests can set
  `promptCacheRetention` to `none` globally.

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

- Default sessions reuse stable prefixes through short-lived provider cache hints.
- Cache and context controls sit at the provider boundary that understands their
  wire formats and caveats.
- Repo-local config cannot silently expand prompt retention or enable server-side
  edits.
- Diagnostics avoid false cache-miss blame and report only proven prefix breaks.

### Negative

- Users must opt out manually when they need provider request cache hints omitted.
- `promptCacheRetention` is a coarse cross-provider knob; exact cache semantics
  still differ between OpenAI and Anthropic.
- Anthropic compact remains unavailable until the transcript/replay contract is
  extended.

### Risks

- Provider APIs may change cache or context-management field names/semantics;
  mitigate with focused provider request-construction tests.
- A future mode/subagent system may need per-worker cache policies; keep the same
  global-owner invariant and pass resolved policy into each worker rather than
  reading repo config directly.
- Provider usage metadata may be incomplete or absent; UI and benchmarks must
  treat missing usage as unknown, not zero savings.

## Addendum (2026-07-10, provider compaction blocks)

ADR-0056 supplies the missing durable representation. Compaction entries may
now persist an adapter-owned `providerBlocks` envelope beside a mandatory,
self-sufficient text summary. `compaction.providerNative=auto` is global-only
and explicitly opts into capability-gated native compaction; the default is
`off`. Mimir replays a block only on the same adapter and exact model; every
other selection uses the text summary. Iris warns when the opt-in is active
because the opaque and portable representations may differ after a model switch.

The earlier blanket rejection of Anthropic compact is therefore lifted for the
normalized provider-native route. The legacy `anthropicContextManagement`
compact field remains rejected so two settings cannot control the same reducer.
Clear-tool-use/local-fold disjointness is unchanged. OpenAI pairs its opaque
encrypted item with a separate provider-authored portable summary before the
native result may enter the common persistence path.

## Addendum (2026-07-05, issue #400; superseded 2026-07-09)

`anthropicContextManagement.clearToolUses` and local microcompaction
(ADR-0048) are mutually exclusive: the server drops tool results Iris still
models as present, so context accounting and fold plans diverge (two reducers
fighting over the same mass). Enforced at selection load
(`ContextManagement::validate_compatible_with_microcompaction`) and at the
`/settings` save boundary (`save_microcompaction`). `clearThinking` remains
compatible: folds never touch thinking blocks, and recall reads Iris's own
log. This discharges epic #379's unfiled "anthropicContextManagement
interplay" gap.

## Addendum (2026-07-09, configurable tool-result compaction)

The blanket exclusion is narrowed to candidate-set overlap. Local reducers and
Anthropic `clear_tool_uses` may run together only when native `exclude_tools`
excludes every local candidate. Semantic dedupe contributes `read` and `ls`;
local clearing contributes its mode/eligibility set. Selection load and nested
live-settings application reject overlap with both setting names and the
conflicting tools.
`clearThinking` remains compatible.

`toolResultCompaction.toolClearing.backend` is global-only for
`anthropicNative` and `auto`. Explicit native clearing fails on other providers;
`auto` falls back to local. Native clearing requires explicit
`includeFailures=true` because Anthropic exposes no result-status filter. Full
original calls/results remain in Iris's JSONL and are recoverable with
`recall(tool_call_id="...")`.
