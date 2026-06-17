# ADR-0014: Tool visibility is not authorization

**Date**: 2026-06-18
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Selectors (ADR-0013) and load/don't-load both control which tools the model sees, creating a risk of conflating "shown to the model" with "allowed to run / needs approval." Iris already separates `Tools::iter` (model-visible) from `Tools::by_name` (executable) in `src/nexus.rs` precisely so a hidden tool stays runnable for resumed transcripts. Approval enforcement lives in Nexus (ADR-0005); effectful tools opt out of persistent allow-always (ADR-0010). Issue #18 introduces untrusted WASM plugin tools. An approval-gate bypass is a blocking-critical defect.

## Decision

Visibility decides only what the model is shown and whether a tool is granted; it never weakens the approval gate. A **policy-denied** tool is both invisible and refused at execution — deny is enforced on the execute path, not by hiding alone, because a call can still arrive from a resumed transcript, a hallucination, or prompt injection. A **capability-hidden** tool (e.g. a provider with native edit hides generic `edit`) stays runnable. A tool's frontmatter/manifest approval level is a request, not a grant: a dropped-in tool can never lower its own approval bar; user/command/plugin tools default to per-call approval, loosened only by explicit user config. Load-vs-filter is a per-source choice — trusted built-ins filter (preserving hidden-but-runnable resume safety); untrusted plugins may lazy-load/drop-on-deny for a smaller resident attack surface — and the execute-time deny check holds under either choice.

## Alternatives Considered

### Visibility is the lock (absent from the visible set = denied)
- **Pros**: Simplest; no separate execute-time check.
- **Cons**: Hidden tools remain executable via `by_name`; resumed/injected/hallucinated calls bypass the intent; deny is unenforced.
- **Why not**: Security hole, and it collapses the capability-hidden vs policy-denied distinction that resume safety depends on.

### Let a tool's frontmatter/manifest declare (self-grant) its own approval
- **Pros**: Fully declarative; less user config.
- **Cons**: A malicious or injected tool file could disable its own gate.
- **Why not**: Approval-gate bypass is blocking-critical; approval must be host-decided.

### Unload-only deny for every tool source
- **Pros**: Strong deny with no execute-time check needed.
- **Cons**: Built-ins cannot be unloaded per mode without a rebuild; breaks capability-hidden resume; plugin reload churn.
- **Why not**: A per-source choice is better; built-ins need filter + execute-block, which pi-mmr also uses (allowlist + tool_call blocking on a loaded set).

## Consequences

### Positive
- The approval gate cannot be bypassed by visibility configuration.
- Resumed transcripts keep working for capability-hidden tools.
- Untrusted plugins can stay unloaded while a mode denies them (smaller attack surface).

### Negative
- For filtered sources, deny must be enforced in two places (visibility and execute).

### Risks
- A future code path could trust visibility for authorization; mitigate with security tests asserting a policy-denied tool is refused on execute (including a simulated resumed-transcript/injected call), a capability-hidden tool still executes, and a tool declaring `approval: none` still prompts.
