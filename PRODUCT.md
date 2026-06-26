# Product

## Register

product

## Users

Terminal-native, highly experienced developers and power users. People who live
in a shell, read diffs faster than prose, and already know how they want a change
made — they are reaching for an instrument, not a collaborator that decides for
them.

Their context when using Iris:

- Working inside a real repository, mid-task, with a concrete change in mind.
- Cost-aware on purpose. The era of unbounded API spend is over; these users have
  to justify agent usage with measurable outcomes, so token cost and context
  discipline are first-class concerns, not afterthoughts.
- Skeptical of "autonomous magic." They explicitly do **not** want a vibe-coding
  harness that runs off on its own. They want direct, honest feedback and full
  control, with every detail available on demand.

The job to be done: *make a precise, reviewable change to my codebase — the diff
I would have written — while spending as few tokens as possible and showing me
exactly what it did.*

## Product Purpose

Iris is a fast, token-efficient coding agent for the terminal. It is a **tool that
serves the developer's work, not a substitute for it.**

What it does and why it exists:

- **Every token is deliberate.** The core is a context engine that budgets,
  justifies, caches, and freshness-checks what reaches the model — explicit
  budgeting and a context ledger over best-effort truncation. Large content lives
  behind typed handles rather than being dumped into the prompt.
- **The diff is the deliverable.** Iris is judged on the diffs, commits, and PRs
  it ships, not on chat quality. The workflow is built around the change, with
  approval gates, diff previews, and (planned) checkpoint/rollback.
- **Honest, not flashy.** No bells and whistles promising agentic autonomy. The
  surface is calm and minimal at first glance — direct feedback, minimal
  distraction — but *all* the detailed information is there in a structured,
  human-readable, toggleable form for when the expert needs it.
- **Efficiency it can prove.** Token savings, cache hits, cheaper mode switching,
  and compaction quality are treated as design goals to be backed by benchmarks
  before they are sold as features. Honesty over hype is a product stance.

Success looks like: an experienced developer reaches for Iris instead of a
heavier autonomous agent because it is precise, predictable, cheap per session,
and gets out of the way — and the token-efficiency thesis is backed by
measurement, not assertion.

## Brand Personality

An honest, precise instrument — deliberately cold and logical. Iris reads like a
piece of human/industrial engineering design (Teenage Engineering lineage):
restrained, mechanical, calm, and confident without theatrics.

- **Three words:** precise, mechanical, honest.
- **Voice:** terse, factual, unsentimental. States what it did; does not perform
  enthusiasm or promise magic.
- **Emotional goal:** the quiet trust of a well-made tool. The user should feel in
  control and informed, never managed or upsold.
- **Restraint with intent:** a muted grey foundation carrying a few sparse,
  role-assigned accents — not decoration, signal. Color and chrome are earned.

## Anti-references

What Iris must **not** look or feel like:

- Chat applications. No role cards, no `USER` / `AGENT` labels, no boxed
  conversation bubbles.
- Dashboards and "mission control." No bottom telemetry/status bars, no
  CPU/MEM/QUEUE readouts, no dense multi-pane cockpit.
- A GUI recreated in text. No tabs, sidebars, decorative widgets, or desktop-app
  chrome rebuilt with box-drawing characters.
- Hype-driven autonomous-agent UX. No "magic," no vibe-coding theater, no
  promising outcomes the benchmarks haven't earned.
- Visual noise. No braille spinners, no rainbow/green-yellow-red meters, no
  color-as-decoration, no over-iconography.
- A framework to build other agents on. Iris is the product a person uses, not a
  library or platform.

## Design Principles

1. **Every token is deliberate.** Inclusion is budgeted and auditable; nothing
   reaches the model by accident or panic.
2. **The diff is the deliverable.** Optimize the surface and workflow around the
   change being shipped, not the transcript.
3. **A tool, not a substitute.** Keep the expert in control. Direct feedback,
   explicit approval, no autonomous overreach.
4. **Progressive disclosure.** Minimal and calm at a glance; complete detail
   structured and toggleable on demand. Nothing important is hidden, nothing
   trivial is shouted.
5. **Terminal-native, not GUI-like.** Tool output earns chrome; conversation stays
   plain. Communicate state through a small, consistent symbol vocabulary.
6. **Honesty over hype.** Claims are backed by measurement; the interface never
   oversells what the runtime actually does.

## Accessibility & Inclusion

- **Never rely on color alone.** State is always carried by symbols and labels as
  well as color; the UI must be fully legible in monochrome.
- **Light and dark terminals.** The palette is built from terminal-relative ANSI
  roles, so it adapts to the user's own light or dark theme rather than hard-coding
  one.
- **Reduced-motion awareness.** Live motion is confined to a single LED-chase
  working indicator and occasional edge-dot pulse; these should degrade to a
  static readout when motion is unwanted.
- **Non-interactive fallback.** A plain text renderer (`src/ui/text.rs`) serves
  pipes, CI, and screen-reader-hostile environments where the interactive TUI is
  inappropriate.

---

*Visual system of record: [docs/TUI_DESIGN_LANGUAGE.md](docs/TUI_DESIGN_LANGUAGE.md)
(exhaustive pane grammar) and [DESIGN.md](DESIGN.md) (impeccable-format summary).*
