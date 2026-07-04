# ADR-0036: Tools are token-efficient by design

**Date**: 2026-07-04
**Status**: accepted
**Deciders**: iris-agent maintainers

## Context

Every byte a tool returns is model context. Verbose tool output shortens sessions,
degrades reasoning quality, and raises cost. External proxy CLIs exist that wrap
dev commands and compress their output before it reaches an agent; they require a
separate install, hook-based command rewriting, and trust machinery — all of which
exist only because those tools do not own the runtime. Iris owns the runtime and
the tool surface, so compression belongs inside the tools, not in front of them.

Iris already applies this partially: native `read`, `grep`, `ls`, and `find`
return bounded windows, `bash` truncates to a tail cap (ADR-0008), and oversized
results move behind session handles (ADR-0011). This ADR makes the principle
explicit so future tools are held to it.

## Decision

Goal: maximum token efficiency without quality loss — every tool result carries
the fewest tokens that preserve full task success. Token efficiency is a design
requirement for every tool result, owned by the tool itself:

- A tool returns the minimum output that lets the model act correctly; complete
  detail stays retrievable on demand (offsets, handles per ADR-0011, explicit
  verbose parameters) rather than emitted by default.
- Native tools structure and bound their own output (line/byte caps, windows,
  match-focused results) instead of relaying raw program output.
- Pass-through surfaces (`bash`) apply output reduction after capture, inside the
  runtime, before the result enters the transcript (see ADR-0037).
- Reduction must never change semantics: exit codes, error text, and failure
  signals are preserved verbatim or summarized only when provably redundant.
- No external filter binary is required or assumed. Iris ships this behavior in
  the Nexus tool tier.

## Definition: token-efficient tool output

A tool result is token-efficient when all five hold:

1. **Success is cheap.** An unremarkable success costs a summary, not a log:
   `ok`, `142 tests passed`, `5 commits, +142/-89` — not the compiler chatter,
   progress lines, or per-test listing that produced it.
2. **Failure is complete.** Everything needed to act survives verbatim: error
   messages, file:line references, failing test names and their output, diff
   hunks, non-zero exit codes. Failure detail is never summarized away.
3. **Noise is zero.** ANSI escapes, progress indicators, spinners, download
   bars, repeated headers, decorative rules, and boilerplate status lines
   (`Compiling …`, `Already up to date`, `Resolving dependencies …`) carry no
   decision value and are stripped entirely.
4. **Detail is on demand, not by default.** Full raw output stays retrievable
   — offsets, session handles (ADR-0011), or an explicit raw/verbose parameter
   — so compression never destroys information, only defers it.
5. **Reduction is measured.** Savings claims cite a benchmark against
   representative outputs. Reference bar for noisy command classes
   (build/test/install/VCS logs): 60–90% token reduction with zero loss of
   actionable content.

Rule of thumb: if a competent engineer skimming the raw output would have
skipped a line, the model should never pay tokens for it; if they would have
read it, it must be present unchanged.

## Alternatives Considered

### Alternative 1: Depend on a user-installed filter proxy CLI
- **Pros**: Zero implementation cost; mature filter sets exist.
- **Cons**: Separate install and update channel; command rewriting via hooks;
  trust/integrity machinery for project-local filters; no coverage for native
  tools; behavior varies with what the user installed.
- **Why not**: Iris owns tool execution; a proxy solves a problem Iris does not
  have and adds a runtime dependency Iris cannot verify.

### Alternative 2: Rely on truncation caps alone
- **Pros**: Already implemented; simple; safe.
- **Cons**: Caps bound the worst case (50 KB / 2000 lines) but keep all noise
  below the cap; a 5,000-token test log under the cap still wastes 90% of its
  tokens.
- **Why not**: Truncation is a memory-safety and context-safety rail, not
  compression. Both are needed.

### Alternative 3: Let the model request less output
- **Pros**: No runtime changes.
- **Cons**: Models reach for default invocations; savings depend on prompt
  discipline and decay over a session.
- **Why not**: Unenforceable; the runtime is the only reliable enforcement point.

## Consequences

### Positive
- Longer sessions and lower cost without user setup.
- One enforcement point (Nexus tools) instead of per-user proxy configuration.
- New tools inherit an explicit standard: bounded, structured, retrievable.

### Negative
- Each tool carries compression logic and tests for it.
- Over-aggressive reduction can hide information the model needed; every
  reduction path needs an escape hatch back to raw output.

### Risks
- Compressing away failure detail breaks debugging loops. Mitigation: error and
  failure lines are exempt from reduction by contract, and reductions are
  covered by tests (tool result/error encoding is a tested behavior class).
