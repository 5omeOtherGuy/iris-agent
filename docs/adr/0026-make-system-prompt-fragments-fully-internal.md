# ADR-0026: Make system-prompt fragments fully internal

**Date**: 2026-07-02
**Status**: proposed (supersedes the file-loading decision of ADR-0012; amends ADR-0013 and ADR-0015)
**Deciders**: Iris maintainers, Pi agent session

## Context

ADR-0012 built the system prompt from user-droppable `.md` fragment files discovered from a global dir (`~/.iris/fragments`) and a per-repo dir (`<cwd>/.iris/fragments`). Repo fragments are attacker-controlled: a fragment lands in the system prompt, the highest-authority instruction channel, so cloning a hostile repo and running `iris` is arbitrary system-prompt injection with zero ceremony. ADR-0012's risk section contained this with a bounded, symlink-refusing reader plus the #234 per-cwd trust gate (`~/.iris/trust.json`, `/trust`, a first-run prompt, a TUI modal).

That entire gate exists to contain a surface Iris does not need:

- Iris rejects the extension-platform model ("a tool, not a framework"; PRODUCT non-goals: no extension API, package manager, or themes). User-droppable prompt fragments are a pi-style extensibility surface — the framework model Iris rejects.
- User and project steering already have a channel: `AGENTS.md`/`CLAUDE.md` are folded in as `<project_context>`, treated as trusted user config (ancestor-walk, never gated). Removing fragments does not remove that.
- Global `~/.iris/fragments` is self-authored and not an injection vector, but it is still a system-prompt-editing surface Iris does not want to own.

## Decision

The shipped fragments in `src/wayland/system_prompt/defaults.rs` become the single source of truth. Iris stops discovering, materializing, and loading `.md` fragments from `~/.iris/fragments` and `<cwd>/.iris/fragments`.

- `assemble()` builds the prompt from the internal fragment set plus the existing dynamic context (`AGENTS.md`/`CLAUDE.md`, cwd, date).
- Fragments stay the internal assembly abstraction. The shared selector schema (ADR-0013) and named slots (ADR-0015) still order and conditionally include fragments per turn — only their provenance changes to in-binary.
- `ensure_default_fragments` (startup disk materialization) is removed. Previously materialized `~/.iris/fragments/*.md` are left orphaned; they are no longer read.
- Removing repo fragment loading eliminates the system-prompt-injection surface, so the #234 trust gate's original purpose ends. The per-cwd trust store is repurposed as a project permission policy in ADR-0027; the fragment-trust decision, the fragment-gating first-run prompt, and issue #255 are obsolete.

The bounded, symlink-refusing reader stays for `AGENTS.md`/`CLAUDE.md`, which are still folded in.

## Alternatives Considered

### Keep global `~/.iris/fragments`, drop only repo fragments
- **Pros**: Preserves a self-authored escape hatch; still deletes the trust gate (only repo fragments are attacker-controlled).
- **Cons**: Retains a system-prompt-editing surface; two provenance classes to load, order, and document.
- **Why not**: `AGENTS.md` already covers user steering, and Iris is deliberately opinionated. The escape hatch does not earn its surface.

### Keep the trust gate and both fragment sources (status quo: ADR-0012 + #234)
- **Pros**: Maximum flexibility; parity with pi's prompt UX.
- **Cons**: Keeps an attack surface plus the trust machinery required to contain it; off-brand for a tool, not a framework.
- **Why not**: The flexibility serves the framework model Iris rejects; the containment cost is pure overhead.

### Remove fragments entirely; hardcode one prompt string
- **Pros**: Simplest possible assembler.
- **Cons**: Loses per-turn, per-provider/model/mode dynamic assembly (ADR-0013, #10, #216).
- **Why not**: The fragment abstraction is still needed internally for dynamic assembly; only its file provenance is being removed.

## Consequences

### Positive
- The system-prompt-injection surface is removed. No untrusted `.md` reaches the prompt.
- The #234 trust gate, `/trust`-for-fragments, and the fragment role of `trust.json` are deleted; #255 becomes obsolete.
- Dynamic per-turn assembly (ADR-0013/0015) is preserved, now over internal fragments whose selector inputs are already internal.
- On-brand: a tool, not a framework.

### Negative
- Users can no longer edit or reorder the system prompt via files; only `AGENTS.md`/`CLAUDE.md` steering remains.
- ADR-0012's user-facing "drop a file" UX is withdrawn; docs referencing `~/.iris/fragments` need updating.

### Risks
- Orphaned `~/.iris/fragments/*.md` on existing installs. Mitigate: stop writing them; note in CHANGELOG. They are inert, not read.
- A user relying on custom fragments loses them silently. Mitigate: a CHANGELOG migration note pointing to `AGENTS.md`.
- Reverses a shipped, closed phase-1 decision (#202/#234). Recorded here so the trade is not re-litigated.
