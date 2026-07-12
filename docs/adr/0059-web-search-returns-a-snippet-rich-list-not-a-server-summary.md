# ADR-0059: web_search returns a snippet-rich result list, not a server summary

**Date**: 2026-07-12
**Status**: accepted
**Deciders**: Iris maintainers

## Context

`web_search` (ADR-0058) must obey ADR-0036: return the fewest tokens that let
the model act correctly, with reduction measured (rule 5). The open question is
what *shape* the result should take, because a competing shape exists in the
market and looks cheaper at a glance.

Iris renders each hit as `title / url / snippet` and ships the ranked list
verbatim (`tool::render_results`). A common alternative — Anthropic's
server-side `WebSearch`, used by Claude Code — returns **bare `title`+`url`
links with no per-result snippet, plus one synthesized summary paragraph** that
its search service composes server-side from page content the client never sees,
plus a "cite your sources" reminder.

We benchmarked both on the same query (`rust async runtime`), same 4-byte/token
estimator, `claude-sonnet-4-6` at medium effort for the external tool. Results
are recorded in `docs/benchmarks/web-tools-token-efficiency.md` and enforced by
`src/tools/web/corpus.rs`:

| | Iris `web_search` (DuckDuckGo) | Claude Code `WebSearch` |
|---|---|---|
| Tool-result tokens | ~751 | ~684 |
| Results | 10 | 9 |
| title+url | ~266 | ~294 |
| per-result snippets | ~485 (all 10) | 0 |
| synthesized summary | none | ~350 |
| reminder footer | none | ~26 |

The headline: **the total token cost is within ~10% — essentially a tie.** The
two shapes spend that near-equal budget on opposite things. Iris spends ~65% on
per-result snippets; the alternative spends ~50% on one blended summary and
gives no snippets at all.

## Decision

Keep the snippet-rich list. `web_search` returns, per result, the title, the
URL, and a short source snippet, ranked, with no runtime-generated summary and
no per-result page bodies. Rationale:

1. **Same cost, more decision signal.** For roughly the same tokens, the model
   sees a preview of every result and can choose which link to open — the job of
   a search tool. Bare links force a blind choice; a snippet per result is the
   material that makes the choice informed.
2. **No summary the model did not generate itself.** A server-side summary is
   digested from content the client never receives, so the model cannot verify
   it, weight its sources, or notice what it omitted. Trusting a black-box
   paragraph is a quality-loss risk ADR-0036 rule 2 exists to prevent: the
   result must carry what a competent reader needs, not a pre-chewed conclusion.
   Any summarizing is the model's own downstream reasoning over visible evidence.
3. **Detail on demand, honestly layered (ADR-0036 rule 4).** The snippet is the
   preview; the full page stays one `read_web_page` away, with objective
   excerpting to compress it. Iris already owns that next hop, so the search
   result does not need to pre-fetch or pre-summarize page bodies.
4. **Reduction stays measurable and owned.** Iris fetches and compresses the raw
   result HTML client-side (~91% on the benchmark fixture, 8096 -> 751 est
   tokens), so the saving is measured at a real seam. A server-side summary has
   no client-visible raw baseline, so its "saving" is unmeasurable and
   unauditable from inside the session.

The snippet cap (Jina backend, `SNIPPET_CHARS`) and the compact renderer keep
this shape token-bounded; the corpus benchmark asserts the reduction floor and
verbatim survival of every result's title+URL.

## Alternatives Considered

### Alternative 1: Server-side summary + bare links (the Claude Code shape)
- **Pros**: a ready-made answer up front; slightly fewer tokens here (~684 vs
  ~751); no snippet-length tuning.
- **Cons**: no per-result snippet, so the model cannot judge which link is worth
  opening; the summary is composed from content the client never sees, so it is
  unverifiable and its omissions are invisible; nothing to measure a reduction
  against.
- **Why not**: near-identical cost buys strictly less actionable signal for a
  coding agent that will open and read a page properly anyway. It optimizes for
  a one-shot human answer, not for an agent choosing evidence.

### Alternative 2: Bare `title`+`url`, no snippets (leanest list)
- **Pros**: the smallest possible list; ~266 tokens for 10 results.
- **Cons**: same blind-choice problem without even a summary to lean on; every
  interesting result forces a follow-up `read_web_page` just to learn what it is.
- **Why not**: the snippet is cheap (~48 tokens/result here) and usually saves a
  wasted read. False economy.

### Alternative 3: Full page content per result (Jina-style raw)
- **Pros**: maximum context per hit.
- **Cons**: blows the token budget; leaks whole pages through a search surface;
  that is `read_web_page`'s job.
- **Why not**: already rejected in the backend design — Jina's full-content hits
  are truncated to snippet length here, never emitted as pages.

## Consequences

### Positive
- One measured, enforced result shape: snippet-rich, ranked, token-bounded,
  with the benchmark (`docs/benchmarks/web-tools-token-efficiency.md`) as the
  regression floor.
- The model always holds the evidence, not a summary it cannot check; any
  synthesis is its own, over visible sources.
- Clean division of labor: search previews, `read_web_page` reads. No pre-fetch
  or nested summarization hidden in the search path.

### Negative
- No instant answer paragraph: a user who wanted a one-line synthesis pays one
  extra model step to get it (the model writes it from the snippets/pages).
- Snippet quality depends on the backend's snippet text (DuckDuckGo markup,
  Jina/Brave `description`), which varies.

### Risks
- Per-result cost scales with snippet length; the snippet cap and the corpus
  reduction bar are the guardrails against drift back toward bloated results.
- Backends that return weak or missing snippets degrade the decision signal;
  the honest empty-snippet path keeps the result truthful rather than padded.
