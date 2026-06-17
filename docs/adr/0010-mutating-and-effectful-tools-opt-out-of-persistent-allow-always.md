# ADR-0010: Mutating and effectful tools opt out of persistent allow-always

**Date**: 2026-06-17
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Nexus owns approval enforcement and the session allow-policy (ADR-0005). The approval gate supports an "always allow this tool" grant that suppresses later prompts for the same tool in a session. For a tool that mutates files or runs shell commands (`bash`, `write`, `edit`), a single such grant could authorize arbitrary later effects with different arguments, because the policy is per-tool, not yet scoped to a path or an exact call. The approval UI also previously offered an "always" choice that Nexus already ignored for these tools, which was misleading.

## Decision

Mutating and effectful tools set `supports_allow_always = false`. The approval prompt omits the "always" option whenever a tool opts out, and the gate refuses an "always" decision for it; only low-effect tools can be persistently always-allowed. The opt-out is a tool capability (single source of truth), so the UI never name-matches `"bash"`/`"write"`/`"edit"` and Nexus keeps enforcing the policy.

## Alternatives Considered

### Allow persistent allow-always for every tool
- **Pros**: Fewer prompts; simplest UX.
- **Cons**: One "always" grant can green-light arbitrary later mutations or shell commands.
- **Why not**: The blast radius of an unscoped persistent grant on an effectful tool is unacceptable until policy is path/exact-call scoped.

### Name-match effectful tools in the UI
- **Pros**: No tool-trait change.
- **Cons**: Safety policy leaks into Tier 3 and drifts from the tool set.
- **Why not**: Classification belongs on the tool; enforcement stays in Nexus (ADR-0005).

### Build path/exact-call scoped allow-always now
- **Pros**: Keeps an "always" affordance without the blast radius.
- **Cons**: Needs a scoping model and storage not yet designed.
- **Why not**: Deferred; opting out is the smaller correct step until scoped policy exists.

## Consequences

### Positive
- A single stray "always" grant cannot authorize future mutations or shell commands.
- Classification rides on the tool while Nexus enforces; composes with ADR-0005 and the capability threading from #41.
- The prompt only offers choices the runtime will honor.

### Negative
- More `y`/`N` friction for `bash`/`write`/`edit` until a scoped allow-always lands.

### Risks
- A future tool that forgets to opt out could be persistently always-allowed by mistake; mitigate with tests asserting mutating built-ins opt out and that allow-always does not cross tool boundaries.
