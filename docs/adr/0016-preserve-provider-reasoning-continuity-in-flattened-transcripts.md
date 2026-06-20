# ADR-0016: Preserve provider reasoning continuity in flattened transcripts

**Date**: 2026-06-20
**Status**: proposed
**Deciders**: Iris maintainers, Pi agent session

## Context

Runtime reasoning selection now exists: a normalized `ReasoningEffort` resolves
through `mimir::selection`, is validated/clamped by `mimir::model_capabilities`,
and each adapter maps it to its provider wire shape. The Anthropic adapter emits
adaptive `thinking` + `output_config.effort`, budget `thinking`, or an explicit
`thinking: { type: "disabled" }`, while the no-preference path stays
byte-identical to today's request.

That closes the request side but not the round-trip. When extended thinking is
enabled and the model uses tools across turns, Anthropic requires the prior
assistant `thinking` / `redacted_thinking` blocks (with their signatures) to be
replayed verbatim on the follow-up request, and it rejects a request whose
latest assistant message has had those blocks modified or dropped. Today the
Anthropic SSE decoder discards `thinking_delta` / `signature_delta`, and
`build_messages` never re-emits them, so a multi-turn thinking + tool-use
conversation produces a malformed request. The reference Claude Code OAuth
provider (`minimalcc-pi`) captures these blocks, replays them byte-for-byte, and
gates replay on the exact producing model; pi-mono behaves similarly, and the
`pi_agent_rust` port carries thinking blocks in a content-block enum.

The structural question is how Iris should carry this provider state.
Iris does not model an assistant message as a typed content-block union. A
multi-part assistant turn is flattened into sequential `Message` rows keyed by
`Role` (`Assistant` text, one `AssistantToolCall` per call, `Tool` result), and
each adapter re-groups consecutive rows back into provider wire format. There is
currently no model identity recorded on a `Message`. This ADR decides the
representation, the ownership of the replay rules, and the persistence shape, so
the Anthropic lane can become correct when it goes live without a transcript
format break later.

Iris's only live provider today is OpenAI Codex Responses, so this path is not
yet exercised in production; the decision is recorded now to fix the seam before
the Anthropic/Claude Code OAuth lane is enabled.

## Decision

Extend the existing flattened-row transcript with one provider-neutral
reasoning row rather than introduce a content-block union.

- **Transcript row.** Add `Role::AssistantReasoning`. The row carries the
  reasoning text in `content` and a provider-neutral payload: an opaque
  `continuity` string (the Anthropic signature, or the `redacted_thinking`
  opaque `data`), a `redacted` flag, and a `ModelOrigin` (`provider`, `api`,
  `model`) identifying which model produced it. Nexus never interprets
  `continuity`; it is opaque round-trip state. Naming stays neutral
  (`continuity`, `origin`) so no Anthropic concept leaks into Tier 1.

- **Ordering.** Within a turn the reasoning rows are flattened before the
  assistant text and tool-call rows, because Anthropic requires thinking to
  precede `tool_use` in the assistant message. Full multi-tool correctness also
  requires that all assistant-emitted rows of one model turn
  (reasoning -> text -> every tool call) stay contiguous before the tool
  results; the current loop interleaves each `AssistantToolCall` with its
  `Tool` result, so either the loop is changed to group them or the limitation
  is documented and the lane kept single-tool.

- **Capture and replay live only in the Mimir adapter.** The Anthropic adapter
  accumulates `thinking_delta` / `signature_delta` / `redacted_thinking` into a
  reasoning row, and on `build_messages` it: replays a same-origin signed block
  byte-for-byte as `thinking` (no trim/sanitize, replayed even when the visible
  text is empty); replays a same-origin redacted block as `redacted_thinking`;
  downgrades a foreign-origin visible block to plain `text`; and drops a
  foreign-origin redacted block. A signature is never replayed to a model other
  than the one that produced it (`provider`+`api`+`model` must match), because
  signatures are model-specific and Anthropic rejects a foreign signature.
  Codex and Antigravity ignore reasoning rows in this slice.

- **Persistence.** Reasoning rows are persisted as ordinary `message` entries
  (not skipped side events like `modelSelection`), with `continuity`,
  `redacted`, and `origin` fields, and are read back on resume. They are counted
  in token estimates (including the opaque `continuity` for redacted blocks),
  and compaction must not split a retained assistant tool-use turn from its
  preceding reasoning rows. If compaction safety is deferred, the Anthropic lane
  stays non-live until it lands.

## Alternatives Considered

### Replace `Message.content: String` with a typed content-block union
- **Pros**: Matches pi-mono / `pi_agent_rust`; models interleaved
  text/thinking/tool/image blocks directly; durable assistant-turn boundaries.
- **Cons**: A Tier-1 rewrite touching every adapter, the agent loop, the session
  format, and all message tests; large blast radius for one provider's need.
- **Why not**: Iris deliberately chose flattened rows (ADR-0001/0004). A new row
  type reuses the existing flatten/regroup and session machinery; the union is
  the right move only when a provider needs arbitrary block interleaving or
  structured reasoning-item replay, which is deferred.

### Drop redacted thinking; replay only signed blocks (the `pi_agent_rust` rule)
- **Pros**: Simplest; no redacted payload to store or model.
- **Cons**: Diverges from the provider we are porting (`minimalcc-pi`) and
  pi-mono, both of which round-trip `redacted_thinking` to the same model; losing
  it weakens multi-turn continuity Anthropic expects to receive back.
- **Why not**: Storing one opaque string plus a `redacted` flag is cheap, and an
  earlier assumption that redacted blocks should be dropped was checked against
  `minimalcc-pi` and found wrong.

### Reuse `tool_call_id` to carry the signature instead of new fields
- **Pros**: No new `Message` field.
- **Cons**: Overloads a tool-framing field with unrelated reasoning state; the
  meaning becomes role-dependent and review-hostile.
- **Why not**: Clarity at a system boundary is worth one explicit optional field.

### Persist reasoning as a skipped side event (like `modelSelection`)
- **Pros**: No change to the message-count / read path.
- **Cons**: A skipped entry is not reconstructed on resume, so the blocks could
  not be replayed after reload, defeating the purpose.
- **Why not**: Continuity must survive resume, so the rows must be
  read-visible and token-counted.

### Record provider/model identity globally instead of per row
- **Pros**: One field instead of per-row origin.
- **Cons**: A session can switch model mid-conversation (the mode-switching
  feature), so a single global identity cannot gate per-block replay correctly.
- **Why not**: The same-model gate is inherently per-block; origin belongs on the
  row that carries the signature.

## Consequences

### Positive
- The Anthropic thinking round-trip becomes correct without a transcript format
  break, reusing the existing flattened-row store and resume path.
- Tier purity holds: Nexus and the session store carry opaque continuity plus an
  origin tag and never interpret a provider signature; all wire and same-model
  rules stay in the Mimir adapter.
- The no-preference request path remains byte-identical, so today's behavior and
  the Codex lane are unaffected.
- The opaque `continuity` + `origin` shape generalizes to other providers'
  reasoning continuity later (subject to the deferral below).

### Negative
- `Message` gains optional reasoning fields that ripple through every
  constructor, the session serializer/reader, and equality, even though only one
  row type uses them.
- Single-string `continuity` does not fit a provider whose reasoning replay needs
  a structured payload (e.g. OpenAI Responses reasoning items); that lane is
  deferred and may force a richer opaque type or the union later.

### Risks
- Multi-tool turns: if the loop is not changed to group all assistant rows
  before tool results, replay can split one model turn across assistant messages
  where only the first carries thinking, which Anthropic may reject. Mitigate by
  fixing the grouping (and answering all trailing unanswered tool calls) or
  documenting the single-tool limit and keeping the lane gated.
- Compaction: counting and not splitting reasoning/tool-use turns is required;
  an unguarded compaction could strip a signed block from a retained turn and
  break the next request. Mitigate with pair-aware range selection (as in
  ADR-0009) extended to reasoning rows, or defer auto-compaction for the
  Anthropic lane until it lands.
- Byte-exact replay is easy to regress (a stray trim/sanitize mutates the signed
  bytes and trips a 400); mitigate with a replay test that asserts the exact
  bytes and the empty-text signed case.
