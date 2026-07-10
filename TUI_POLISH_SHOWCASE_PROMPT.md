# TUI Polish Showcase — Model Comparison Prompt

You are being evaluated head-to-head against another frontier model. Both of you receive this exact prompt and the same codebase. Your output will be published on my blog and compared side by side in front of 500,000+ readers, with full credit to you. This is a public demonstration of design taste, engineering judgment, and the ability to carry complex, real-world work over the finish line. Treat it accordingly.

## The Task

Optimize and polish the TUI of our Coding Agent Harness (Rust + ratatui). The TUI is already solid and follows a distinct design language. Your job is not a redesign — it is the last 10%: the part that separates "good" from "unmistakably crafted."

## Artistic Direction (non-negotiable)

Every interaction must feel **tactile** — like operating a small, beautifully engineered machine. Every button, output, menu, and setting must be legible, coherent, and functionally clear. One unbroken artistic direction from surface to interaction: a real instrument from a world where **mechanical precision meets AI**.

Translate that into concrete rules and write them down before you code:

- Motion is subtle, fast (roughly 80–200ms), eased, and always *reactive* — animation acknowledges user input, never decorates idly.
- Visual hierarchy guides the eye to what matters *right now*: active tool calls, streaming output, errors, pending confirmations. Everything else recedes.
- Information density adapts to state: dense when scanning history, focused when something is happening, minimal when idle.
- Nothing flickers, jumps, or reflows unexpectedly. Stability is part of tactility.

## Required Process

**Phase 1 — Audit.** Before changing anything, walk the entire TUI and produce a written findings list: visual inconsistencies, jank, bugs, spacing/alignment errors, color misuse, unclear states, UX friction. Number every finding. Hunt for the obscure ones — the bugs that only show up in edge cases.

**Phase 2 — Implement.** Fix the findings and elevate the design. Use ratatui to its full potential: custom widgets, layered rendering, gradient/ramp effects within terminal constraints, precise Unicode box work, stateful animations driven by the event loop. Give **tool calls and their rendered outputs special care** — they are the heart of this product. Their lifecycle (pending → running → streaming → success/failure → collapsed history) should be the most refined sequence in the app.

**Phase 3 — Iteration passes (minimum 3).** After the implementation is "complete," go through the entire TUI again with a fine-toothed comb. Each pass must produce a written changelog: what you found, why it mattered, what you changed. A pass that finds nothing is a failed pass — look harder. Do not present final results until all three passes are documented.

## Edge Cases You Must Explicitly Handle and Test

- Minimum terminal size (80×24) and live resizing mid-stream
- Very long lines, word wrap, and horizontal truncation with correct ellipsis
- CJK characters, emoji, combining characters (width calculation!)
- 16-color and 256-color terminals: graceful degradation of your palette
- Empty states, first-run states, error states, interrupted/cancelled tool calls
- Extremely fast streaming output (no tearing, no scroll jitter)
- Scrollback position preservation during new output
- Focus states: it must always be obvious what has keyboard focus and what keys do

## Deliverables

1. The full code changes (as diffs or complete modified files)
2. `DESIGN_NOTES.md`: your design principles, the audit findings list, and all three iteration-pass changelogs
3. A bug list: every bug found, with cause and fix
4. A short "director's commentary": the 5 changes you're proudest of and why

## Acceptance Rubric — self-grade before submitting

- [ ] Zero visual regressions; everything that worked still works
- [ ] Every animation is interruptible and respects a reduced-motion setting
- [ ] The eye lands on the right thing within 200ms of any state change
- [ ] Tool call rendering reads clearly at a glance in a 40-message history
- [ ] All edge cases above verified, not assumed
- [ ] The three iteration passes each produced real, documented improvements

Do not compromise on detail. Depth beats breadth: a smaller set of changes executed flawlessly will beat a large set executed at 95%. Show what you're capable of.

This is your task! Set your goal accordingly, save the prompt as a file so you can reference it when the session get's longer.

Start! It is your time to shine!
