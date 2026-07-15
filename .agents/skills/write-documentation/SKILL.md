---
name: write-documentation
description: Repo documentation style guide for Iris. Use when writing or editing any repo doc — README, PRODUCT/DESIGN, architecture, TUI specs, features, roadmap, ADRs, codemaps, troubleshooting, or code comments. Enforces the terse field-manual voice, progressive disclosure, measured claims, placeholder examples, and per-doc-type templates.
---

# Write Documentation

Iris docs are instrument manuals. Write for experienced terminal users who want exact
control, reviewable changes, and complete detail on demand. A good Iris doc reads like a
technical field manual: calm, exact, short where possible, complete where necessary.

This skill is the standard. Apply it when authoring or revising any repo document. For
codemap generation specifically, pair with the `documentation-codemap-specialist` skill.

The templates in this skill are the source of truth — not any existing file. The repo's
current docs (`README.md`, `PRODUCT.md`, `DESIGN.md`, `docs/*`, `docs/adr/*`) predate these
rules and are not yet conformant. Do not open them to copy their structure or tone; treat
them as candidates for the editing pass below.

## When to use

- Writing a new doc (`README.md`, `PRODUCT.md`, `DESIGN.md`, `docs/*`, `docs/adr/*`).
- Editing an existing doc — run the editing pass below.
- Reviewing a doc PR for tone, density, and unproven claims.
- Writing doc-comments or product-rule comments in source.

## The standard

```text
precise purpose
minimal surface
structured detail
no hype
no ornamental prose
```

## Voice

Terse, factual, mechanical, honest, unsentimental. State what the thing does.

| Write | Avoid |
| --- | --- |
| Iris stores sessions as JSONL. | Iris revolutionizes agentic development. |
| Mutating tools require approval. | Unlock a magical new coding workflow. |
| The TUI hides raw trace by default. | Seamlessly supercharge your productivity. |
| Run `cargo test` before changing renderer layout. | This powerful feature makes coding effortless. |

No emoji in docs (repo rule), no marketing copy, no retroactive justification.

## Cut at the line level

Once the structure is right, tighten the prose. Cut:

- Adverbs — let the verb carry it.
- Qualifiers — "very," "really," "quite," "somewhat."
- Throat-clearing — "it's important to note that," "it should be said that."
- Inflated phrases — "at this point in time" -> "now."
- Redundancy — "past history," "completely finished."
- Dead metaphors — if you have heard it, cut it.

Test each word: does it change the meaning? would the reader miss it if cut? is there a
shorter way? If all three are no, cut it.

Keep what carries meaning. Technical adjectives ("JSONL transcript," "inward dependency
direction") and legitimate passive (the system is the implied actor) stay. Brevity serves
precision; it does not replace detail — leave docs complete where completeness matters
(safety, cost, limitations, invariants). Cut words, not coverage.

## Rules

- Lead with the operational fact.
- One sentence per idea; one paragraph per concept; one section per user question.
- Prefer structure (tables, checklists, mockups) over prose.
- Put quick paths before internals. Open with internals only in an explicitly internal doc.
- Keep claims measurable. No unproven claims (see Claims).
- Use placeholders in examples unless documenting real source paths (see Examples).
- Pair every mythology name with its concrete responsibility (see Naming).
- Progressive disclosure: README -> feature doc -> spec -> source.
- Never hide safety, cost, or limitations.

## Progressive disclosure

Every doc follows the same shape as the TUI: minimal by default, complete on demand.

```text
1. what this is
2. when to use it
3. quick path
4. details
5. edge cases
6. links to deeper docs
```

## Density by doc type

```text
README             low-to-medium, quick scanning
PRODUCT.md         medium, principles and product stance
DESIGN.md          medium, summary and tokens
TUI spec           high, exact rendering rules
Architecture docs  high, ownership and invariants
Roadmap            medium, gates and evidence
ADRs               medium, decision records
Codemaps           high, source-grounded navigation
Troubleshooting    low, symptom -> cause -> fix
```

Dense, not packed: every paragraph earns its space. Turn long feature inventories into
tables, checklists, or links. Do not let a status section become a wall of implementation
detail.

## Templates

### README — the front panel

Answers: what is Iris, who is it for, what works today, how do I run/configure it, where next.

```markdown
# Iris Agent

One-sentence product definition.

## Status
Current gate + next gate. Short.

## Install / Run / Login / Configure
One command each; minimal settings example.

## Test
cargo test

## Architecture
One paragraph + links.

## Documentation
Curated link list.
```

Status stays scannable — implemented vs next, not one dense paragraph:

```markdown
## Status

Current gate: prove token-efficiency with benchmark evidence.

Implemented:
- Interactive TUI with transcript replay.
- Provider switching for OpenAI Codex, Anthropic, Antigravity.
- Workspace tools: read, write, edit, bash, grep, find, ls.
- Approval gates with diff previews.
- JSONL transcript persistence and resume.

Next:
- Token-efficiency benchmark proof.
- Persistent approval policies.
```

Keep implementation history out of the README. Move it to `docs/FEATURES.md` or `docs/ROADMAP.md`.

### PRODUCT — principle / why / implication

```markdown
## Principle
One sentence.

## Why
One paragraph.

## Implication
Concrete product consequences.
```

Example — "The diff is the deliverable": show diffs before mutation, keep approvals explicit,
prefer reviewable edits over broad rewrites, benchmark claims before marketing them.

### Architecture — tiered and operational

Name the tier, then its responsibilities. Owns / does not own.

```markdown
## Nexus
Runtime core.

Owns:
- turn loop
- provider event normalization
- tool execution contract
- cancellation
- transcript events

Does not own:
- terminal rendering
- session filesystem layout
- provider OAuth flows
```

### Design spec — spec, not essay

```markdown
# Component Name

## Purpose         what it communicates
## Canonical form  text mockup
## Rules           short bullets
## States          all supported states
## Width behavior  wrap/truncate rules
## Do / Don't      implementation guardrails
## Tests           snapshot cases
```

Mockups use placeholders, never real local paths:

```text
path/to/file.rs    ~/project    git main    {command}
```

### Feature — user-visible first, implementation second

```markdown
# Feature

## What it does
## User-visible behavior
## Commands / settings
## Safety rules
## Implementation notes
## Tests
```

### Roadmap — gate-oriented

A milestone is done when its acceptance gate is satisfied, not when code exists.

```markdown
## Milestone N — Title

Gate:
- The acceptance condition.

Evidence:
- Benchmark runs, deltas, failure analysis, repro command.

Status:
- What is implemented vs pending.

Next:
- Concrete next actions.
```

### ADR — strict and short

```markdown
# ADR: <decision>

Status: accepted

## Context             what forced the decision
## Decision            the choice
## Consequences        easier / harder
## Rejected alternatives   only real ones
```

### Code comments — why, not what

```rust
// Keep timeout out of the rendered command so copy/paste preserves the shell input.
// Header rows reserve the right state slot before truncating meta text.
// READ/GREP/LS/FIND are exploration subevents, not top-level panels.
```

Not: `// This function renders the shell command.`

## Examples

Use neutral placeholders:

```text
~/project    path/to/file.rs    docs/ROADMAP.md    {command}
```

Avoid real local paths (`~/projects/iris-inline-messages`, `/home/...`). Real source paths
are allowed only in source-grounded docs: codemaps, `CONTRIBUTING.md`, captured transcripts.

## Claims

No unproven claims. Until benchmark evidence exists, write the goal, not the result.

```text
Write:  Goal: reduce prompt tokens while preserving task success.
Avoid:  Iris dramatically reduces token usage without quality loss.
```

## Naming

Names are labels, not explanations.

```text
Write:  Wayland is the harness layer. It owns sessions, config, execution environment, tool state.
Avoid:  Wayland guides the light of Iris across the system.
```

## Links

Links are escape hatches for detail. Link text says what the reader gets.

```text
Write:  See [TUI design language](docs/path/to/spec.md) for the full pane grammar.
Avoid:  You can learn more here.
```

## Editing pass

When revising any existing doc, apply in order:

```text
1. Delete hype.
2. Split dense paragraphs.
3. Move implementation detail downward.
4. Replace real local paths with placeholders unless source-specific.
5. Add exact commands.
6. Add acceptance criteria.
7. Link deeper docs instead of duplicating them.
8. Check that every heading answers a user question.
9. Cut at the line level (adverbs, qualifiers, throat-clearing, inflated phrases, redundancy).
```

## Do / Don't

- Do lead with the fact, keep claims measurable, prefer structure, link deeper docs.
- Don't write marketing copy, mix status + implementation + roadmap + rationale in one
  paragraph, duplicate a deeper doc, or use a mythology name without its responsibility.

The result should feel like Iris itself: plain language flows lightly; tools and details
are structured; everything extra is available, but nothing trivial is shouted.
