# Iris — Competitor Analysis

> Multi-source, fact-checked competitor analysis for **Iris**, a minimal Rust
> coding-agent CLI harness focused on speed, clarity, and token efficiency
> (token caching, micro-compaction, dynamically assembled system prompts,
> provider-agnostic across Anthropic/OpenAI/Gemini-compatible backends,
> mode-based workflows inspired by pi-mmr).
>
> **Method.** Deep-research harness: 6 search angles → 23 sources fetched → 99
> claims extracted → 25 verified by 3-vote adversarial check (24 confirmed, 1
> killed). Findings below are labeled by confidence and vote. Coverage was deep
> on the emerging Pi/pi-mmr lineage and Rust reimplementations, and **thinner on
> the big incumbents** (Claude Code, Codex CLI, Cursor, Gemini CLI, Cline,
> Hermes) — those are flagged. Items drawn from background knowledge rather than
> the verified corpus are marked **[bg]**. Date: 2026-06.

---

> **Update (2026-06-14): verified incumbent matrix.** A primary-source pass now
> backs the incumbent rows that were previously **[bg]**. See
> [`COMPETITOR_MATRIX.md`](COMPETITOR_MATRIX.md) for the auditable, date-checked
> matrix. Key corrections/confirmations:
> - **Codex CLI is now 96% Rust and "going native"** (zero-dependency binary) —
>   so Rust + single-binary is table-stakes *even at the incumbent tier*; this
>   reinforces (does not weaken) the refuted "no Rust competition" claim in §7.
> - **Gemini CLI** — TypeScript/Node (not a native binary), **single-provider
>   (Google only)**, ~105.3k stars, very mature.
> - **`pi_agent_rust` already ships content-hash anchored edits** (`hashline_edit`,
>   LINE#HASH) — so anchored edits (§4) are **parity, not a differentiator.**
> - **opencode split confirmed:** `opencode-ai` archived (MIT, Go); successors are
>   **Crush** (Go, **FSL-1.1-MIT** source-available) and **sst/opencode** (TypeScript,
>   MIT, 75+ providers reported).
> - **Provider prompt-caching is not open whitespace.** The verified matrix now
>   documents prompt caching for Claude Code and Aider, and cache-adjacent Auto
>   Compact behavior for Cline. Iris may still differentiate on transparent,
>   cache-aware layout and measurement, but not on merely having caching.
> - **`pi-mmr` confirmed as the Iris author's own 1-star, 16-day-old repo** —
>   conflict of interest; not an independent competitor.
> - **Still gaps:** some exact agent-mechanics cells remain unverified in the
>   matrix, especially edit protocols, repo-map implementations, and
>   prompt-caching details for Cursor, Hermes, Crush, sst/opencode, Codex, and
>   the Pi rows. Do not fill those cells from impression.

> **Update (2026-07-07): subagent / isolation / safety ranking.** A local
> reference pass over **Grok Build CLI 0.2.82** materially changes the subagent
> comparison. See [`.iris-reference/grok-worktree-subsystem-spec.md`](../.iris-reference/grok-worktree-subsystem-spec.md)
> for the observed CLI/runtime evidence. This addendum scores implementation
> quality, not model quality or brand reach.

### 2026-07 subagent and safe-execution ranking

Rubric: **Subagent model** = delegation contract, background/resume behavior,
typed capabilities, and nesting controls. **Isolation safety** = whether child
mutation is contained until explicit apply. **Smoothness** = CLI/TUI management,
progress, recovery, and cleanup. Scores are 1-5 and evidence-backed only where
noted.

| Rank | System | Subagent model | Isolation safety | Smoothness | Verdict |
|---:|---|---:|---:|---:|---|
| 1 | **Grok Build CLI** | **5** | **5** | **5** | Best observed implementation. Its subagent input includes typed worker kind, capability mode, background mode, resume, depth limits, and worktree isolation; child mutations stay out of the parent until explicit apply; lifecycle has durable records plus list/show/rm/gc management and fast snapshot fallbacks. |
| 2 | **Claude Code** | **5** | 3 | 4 | Best productized subagent UX and configurability: built-in/custom subagents with model/tools/MCP/background/isolation frontmatter. The public docs verify rich delegation, but this pass has less evidence of Grok-style durable worktree isolation/apply semantics. |
| 3 | **Cline** | 4 | 4 | 4 | Strong safe-research pattern: read-only subagents launched in parallel by default. Safer than general read-write delegation, but narrower; it is not the same as isolated mutable worktrees with explicit apply. |
| 4 | **pi-mmr / AMP-style routing** | 4 | 2 | 4 | Cleanest routing concept: subagents-as-tools plus per-worker model routing and grouped background workers. Safety is weaker than Grok because pi-mmr's known implementation uses cold child processes and does not provide a durable worktree apply boundary by default. |
| 5 | **Aider** | 1 | 4 | 4 | Not a subagent leader, but still one of the cleanest safe-change workflows: git is the unit of work, repo map is mature, and undo/diff/commit ergonomics are excellent. |
| 6 | **Codex CLI** | 2 | 4 | 3 | Strong sandbox/runtime reference, weak verified subagent surface in this corpus. Use it for async/cancellation/tool-safety design, not as the subagent UX target. |

**Best subagent implementation:** **Grok Build CLI**, narrowly over Claude Code,
because Grok combines model-facing delegation with a concrete mutation-isolation
primitive. Claude Code appears more mature as an ecosystem feature; Grok is the
cleaner systems design for safe agentic coding.

**Cleanest safe-test / safe-execution workflow:** **Grok Build CLI** for isolated
read-write experimentation, then explicit apply. The winning pattern is not just
"run tests safely"; it is **spawn candidate worktrees, let them mutate and test in
isolation, compare outputs, and apply one winner**. Aider remains the benchmark
for simple single-agent git hygiene; Cline is safer for read-only research.

**Most important Iris lesson:** make worktree isolation a runtime primitive, not a
prompt convention. Iris has shipped the read-only subagent backend contract (#460);
the minimum useful mutable backend slice is linked git worktrees, durable ids,
progress notifications, explicit apply, and `list/show/rm/gc`. Fast Btrfs/overlay
snapshots are desired follow-ups after the semantics are correct.

## Executive summary

Iris enters a crowded field where its three headline differentiators — Rust
performance, token efficiency (compaction/caching), and provider-agnostic
multi-backend support — are **already individually shipped by competitors,
including tiny Rust clones**. Mature established competitors (Aider, ~46.2k stars;
opencode/Crush; the broader Codex/Gemini/Cline ecosystem) and an emerging
Pi/pi-mmr lineage (badlogic/pi-mono, earendil-works/pi, oh-my-pi/omp) define the
table-stakes bar: provider-agnostic cores (10–40+ providers), automatic context
compaction, git integration, and codebase mapping.

The strongest validation of Iris's product thesis is **`pi_agent_rust`** — an
authorized zero-unsafe Rust port of Pi that already ships a single binary,
sub-100ms startup, 10 native provider modules, and turn-boundary compaction. That
means **"Rust + lean + token-efficient" alone is not a USP.** For the *runtime
implementation*, however, the stronger Rust reference is **Codex CLI**: mature
Tokio streams, cancellation tokens, stream/tool cancellation races, and guarded
parallel tool execution. Iris's most defensible differentiation lies in
**pi-mmr-inspired mode-based workflows + subagents-as-tools with multi-model
routing**, combined with token-efficiency techniques — *executed on a runtime
Iris owns* — rather than in raw Rust speed or basic compaction. The academic
taxonomy (arXiv 2604.03515) confirms compaction is an architectural requirement
and a live design frontier, so Iris should treat micro-compaction as
**table-stakes-done-better, not a novel selling point.**

**The headline finding is a refutation:** the comforting assumption that a
from-scratch Rust harness has essentially no Rust-native competition (except
Codex CLI) was **killed 0–3** against primary sources. There is already a
populated cluster of Rust-native coding agents.

---

## 1. The competitor landscape

### Direct conceptual competitors — the Pi / pi-mmr lineage
*(Iris's actual design space.)*

- **badlogic/pi-mono** (a.k.a. earendil-works/pi) — TypeScript monorepo: unified
  multi-provider API (`pi-ai`), agent core, coding-agent CLI, differential-render
  TUI. Minimalist core with "powerful defaults but skips features like sub agents
  and plan mode" (added via **extensions**, not core forks). Runs in four modes
  (interactive, print/JSON, RPC over LF-delimited JSONL, SDK). Provider-agnostic
  across 30+ providers. **USP: radical minimalism + extensibility + clean
  embeddable surface.** By default gives the model four tools (read, write, edit,
  bash); advocates "CLI tools with a README the agent reads on demand" for
  token-efficient, progressive tool disclosure.
- **oh-my-pi / omp** (can1357) — a Pi fork rewritten coding-first, "a coding agent
  with the IDE wired in": LSP on every write, real debuggers (lldb/dlv/debugpy)
  via DAP, browser automation. 40+ providers, 32 built-in tools, ~55k-line Rust
  core (TS surface — language is ambiguous, see caveats). **USP: deep IDE/
  toolchain integration + content-hash anchored edits ("hashline").**
- **pi-mmr** (this project's own TypeScript predecessor; 5omeOtherGuy/pi-mmr, MIT)
  — a **Multi-Mode Routing** extension package *inside* Pi. `/mode <smart |
  smartGPT | rush | large | deep | free>` swaps the **whole harness profile**
  (model preferences, thinking policy, context profile, tool allowlist, worker
  profile, prompt behavior) in one reversible command. Ships **subagents as
  first-class tools** (`finder`, `oracle`, `librarian`, `Task`, custom `sa__*`),
  each with its own model/thinking/tool profile, folded into **multi-model
  routing** (`MmrSubagentProfile` + `selectMmrModelRoute` pick a worker's model
  *without changing the parent's*). Background fleet (`task_poll`/`task_wait`/
  `task_cancel`) with a live grouped orchestration board. **Structural ceiling:**
  as a guest in Pi's runtime, subagents run as **cold child processes**
  (`--mmr-subagent`); the in-process runner is a fail-closed stub
  (`MMR_IN_PROCESS_SUBAGENT_RUNNER_AVAILABLE = false`) and `maxTurns` is
  unenforced metadata. **This ceiling is Iris's primary opportunity** — see the
  USP section. *(Source: direct reading of the pi-mmr repo, not the deep-research
  corpus.)*

### Closest Rust-native competitor
- **`pi_agent_rust`** (Dicklesworthstone) — **the single closest direct
  competitor.** Authorized Rust port of Mario Zechner's Pi Agent, `#![forbid
  (unsafe_code)]`, single static binary (no Node/Bun), claimed <100ms startup,
  <50MB idle, ~21.1 MiB binary, stable SSE streaming, 8 built-in tools.
  Provider-agnostic via **10 native provider modules** (anthropic, openai,
  openai_responses, gemini, cohere, azure, bedrock, vertex, copilot, gitlab) +
  custom providers via `models.json`. **Automatic compaction** when estimated
  tokens exceed `context_window − reserve_tokens`, summarizing older messages at
  user-turn boundaries and storing a `Compaction` entry in session JSONL. ~1k+
  stars, active 2025–26. *Performance figures are vendor self-reported.*
  **It proves "Rust + lean single-binary + token-efficient + provider-agnostic"
  is not by itself a USP** (high confidence, 3-0).

### Other niche / clean-room Rust reimplementations
- **Claurst** (Kuberwastaken) — ~98% Rust **clean-room** reimplementation of
  Claude Code built from spec analysis ("never referencing the original
  TypeScript"; cites Phoenix v. IBM precedent). ~8.6k stars, beta v0.1.5.
  Provider-agnostic (Anthropic, Gemini, OpenAI Codex + 30+ via `/connect`).
- **keon/mini-claude-code** — 100% Rust minimal Claude Code clone (Tokio,
  ratatui, reqwest), ~8 stars / 1 commit. **Even this toy already ships
  auto-compaction at ~167k tokens and status-bar token tracking** — illustrating
  how commoditized these features are.

### Established incumbents
- **Aider** (Aider-AI/aider) — ~80% Python, Apache-2.0, **~46.2k stars / ~4.6k
  forks**, actively maintained (pushed 2026-05). **USP: tree-sitter ranked-symbol
  whole-repo map + deep automatic git integration** (auto-commits with sensible
  messages; `git diff`/`undo` as the default unit of work). Provider-agnostic via
  LiteLLM (Claude, OpenAI o1/o3-mini/GPT-4o, DeepSeek, Gemini, local/Ollama, any
  OpenAI-compatible API). Directly overlaps Iris's positioning and **defines the
  repo-aware-editing table-stakes bar.**
- **opencode** — ⚠️ **brand is split.** The original `opencode-ai/opencode` is
  **archived (Sep 18 2025)**; it was Go (99.2%, Bubble Tea TUI), not Rust, and was
  provider-agnostic across 8+ backends. Maintained successors: **Charm's Crush**
  and **sst/opencode** (advertising 75+ providers; ~170k stars per one verifier
  note). Target the living forks, not the dead repo.
- **AMP Code CLI** [bg/secondary] — ships **pi-mmr-style mode routing in
  production**: distinct modes (Smart/Deep/Rush) and subagents (Oracle,
  Librarian, Search, Review) each routed to different models (e.g. Oracle on a
  high-reasoning GPT tier, Search on a fast cheap model). Confirms the
  mode-routing + subagent product is commercially validated — and notably **is
  the one shipping pi-mmr-style modes, which `pi_agent_rust` does not.**
- **Claude Code** [bg] — the reference design; USP = agentic quality, subagents,
  hooks, MCP, plan mode, polish.
- **OpenAI Codex CLI** [bg] — Rust/TS, ~72k stars; USP = OpenAI-model-tuned,
  sandboxed execution. The one mature Rust-based CLI agent.
- **Gemini CLI** [bg] — TypeScript, ~100k stars; USP = large free tier + 1M
  context.
- **Cline** [bg] — TypeScript, ~60k stars; USP = VS Code-native, plan/act modes,
  MCP marketplace.
- **Cursor / Cursor CLI** [bg] — best-in-class IDE editing UX, proprietary.
- **Hermes / "Everything Claude Code 2"** [bg] — emerging; the corpus only grazed
  these (one blog). **Treat as unverified** — see open questions.

---

## 2. USP summary table

| Competitor | USP |
|---|---|
| pi-mono / Pi | Radical minimalism + extension model + clean embeddable surface |
| oh-my-pi (omp) | IDE wired in (LSP/DAP/browser) + content-hash anchored edits |
| pi-mmr | One-command whole-harness mode switching + subagents-as-tools + multi-model routing (riding inside Pi) |
| `pi_agent_rust` | Authorized Rust port of Pi: single binary, fast startup, 10 providers, compaction |
| Claurst | Clean-room Rust reimplementation of Claude Code |
| Aider | Whole-codebase tree-sitter repo map + deep git integration |
| opencode/Crush/sst | Broad provider support (75+), maintained TUI lineage |
| AMP Code CLI | Production multi-model routing across modes + specialized subagents |
| Claude Code | Agentic quality, subagents, hooks, MCP, plan mode |
| Codex CLI | OpenAI-tuned, sandboxed, Rust |
| Gemini CLI | Free tier + 1M context |
| Cline | VS Code-native, plan/act, MCP marketplace |

---

## 3. Eventual table-stakes in the market

These are the features the *market* converges on over time — **not** Iris MVP
requirements. The MVP is deliberately narrow (see `FEATURES.md`); the list below is
what Iris would need to reach to be at parity with mature tools eventually, not
what the first milestone must ship. The research is unambiguous that an 8-star toy
clone already has most of these:

1. **Provider-agnostic core, 10–40+ providers.** Anthropic/OpenAI/Gemini is the
   *minimum*. The bar is 10 native (`pi_agent_rust`) to 40+ (omp) to 75+ (sst).
2. **Automatic context compaction.** Confirmed an *architectural requirement*:
   arXiv 2604.03515 found the one agent without it (mini-swe-agent) crashes on
   `ContextWindowExceededError`.
3. **Token-usage tracking** in the UI.
4. **Git integration** — Aider-grade auto-commit + diff/undo.
5. **Codebase mapping** — tree-sitter ranked-symbol repo map.
6. **Stable SSE streaming, single binary, fast startup** — `pi_agent_rust` set
   this bar in Rust already.
7. **Core edit/read/shell tool set** (8–32 tools across the field).

---

## 4. Specialties worth copying

- **omp — content-hash "hashline" anchored edits** ⭐ — model points at
  content-hash anchors instead of retyping lines; self-reported **~61% output-
  token cut on Grok 4 Fast, ~30% on Claude Opus.** The single most concrete,
  copyable token-efficiency win found, and it's *beyond* compaction.
- **pi-mmr / AMP — subagents-as-tools + per-worker multi-model routing** — search
  on a cheap model, advisor on a high-reasoning model, parent unchanged.
- **Pi — "CLI tools with a README" progressive tool disclosure** — pay tool-
  description tokens only when a tool is actually needed.
- **Aider — repo map + git-as-the-unit-of-work.**
- **omp — IDE wired in** (LSP-on-write, real debuggers).
- **`pi_agent_rust` — turn-boundary compaction** writing a `Compaction` entry into
  session JSONL (clean, inspectable).

## 5. What to learn from each
- **Codex CLI:** use as the primary Rust reference for finishing Nexus's async
  runtime: Tokio provider streams, cancellation tokens, child cancellation, and
  guarded parallel tools.
- **`pi_agent_rust`:** the performance pitch is already matched — don't lead with
  it, and do not copy its bespoke runtime or monolithic structure.
- **mini-claude-code / Claurst:** the Rust-clone niche is real and crowded;
  differentiate on design, not language.
- **arXiv 2604.03515:** compaction is a *live frontier* (7 distinct strategies) —
  quality of compaction is a legitimate place to win.
- **Aider:** repo-aware editing + git ergonomics are what users actually reward.
- **pi-mmr/AMP:** mode routing + subagents are a validated product shape; the
  open question is execution (in-process vs. child-process).
- **opencode:** brand/maintenance matters — don't anchor on dead repos; pick a
  defensible name.

---

## 6. Iris's USP — current vs. recommended

**ICP (decided 2026-06): end-user coding CLI** — the tool a developer runs in their
terminal, *not* a foundation/SDK for building agents. So positioning is the
*experience* (cost per session, latency, useful context, diff-centered workflow),
and the competitors that matter are Claude Code, Cursor/Cursor CLI, Aider, Codex
CLI, Gemini CLI, Cline, and the Pi/pi-mmr lineage.

**Current (as stated):** Rust speed + token efficiency + provider-agnostic +
pi-mmr modes. **Problem:** `pi_agent_rust` already bundles *three* of these — Rust
single-binary speed, provider breadth, and compaction. It does **not** ship
pi-mmr-style mode/subagent routing (that's shipped by **AMP Code CLI**, not by
`pi_agent_rust`). So three of the four planks are commoditized; only the
modes/subagent-routing plank is contested whitespace among the lean Rust/Pi-lineage
tools. The first three alone give Iris **no defensible USP.**

**Recommended (medium-confidence synthesis).** Stop selling raw Rust speed and
"we have compaction." Differentiate on the combination of:
1. **pi-mmr-inspired mode-based workflows** + **subagents-as-tools with
   multi-model routing** — the plank with the least Rust-native competition. Iris
   owns its runtime, so it can run subagents **in-process**, the seam pi-mmr defers
   (`MMR_IN_PROCESS_SUBAGENT_RUNNER_AVAILABLE = false`). **In-process is the
   enabler, not the headline:** it removes the IPC/serialization boundary, making a
   shared content store, handle-passing, and live per-worker budgets **simpler,
   lower-overhead, and more centrally enforceable** (child processes *can* share
   state via files/IPC — just more awkwardly and with weaker central control, not
   impossibly). The customer-visible wedge is those *capabilities*, not "in-process"
   itself (process startup is negligible next to model latency).
2. **Token-lean tool contracts** — content-hash / anchored edits so *output*
   tokens drop, not just context. *(Research — spec it first.)*
3. **Micro-compaction with measurable quality** — verification probes + freshness
   checks; treat as table-stakes done well. Claim "better" only once benchmarked.
4. **Cache-aware prompt layout + provider prompt-caching** surfaced explicitly.
   *Whether competitors under-exploit this is unconfirmed — verify in the matrix.*

**The wedge question to answer:** given `pi_agent_rust` already occupies
"authorized Rust port of Pi with compaction + 10 providers," what can Iris ship
that the Pi lineage and its Rust port cannot? **Current best answer (a hypothesis,
not a proven advantage):** budgeted, handle-returning subagent delegation — workers
under enforced token/turn budgets returning **handles + micro-summaries** over a
shared content-addressed store (pointers, not payloads *in the coordination case* —
handles don't let a model reason over unseen bytes), with isolation as the default
and context-sharing as an opt-in dial on the ledger; in-process execution is the
enabler. **This must be demonstrated by an MVP demo + token/latency/task-success
benchmark before any external (investor/customer) use.**

---

## 7. Refuted claim (killed 0–3)

> "Among CLI coding agents analyzed, ... the only Rust agent [is] OpenAI's Codex
> CLI ... a from-scratch Rust harness like Iris has essentially no direct
> Rust-native competitor among mature CLI agents except Codex CLI."

**Refuted** against arXiv 2604.03515 and the repos above. Iris **does** face
Rust-native competition: `pi_agent_rust`, Claurst, mini-claude-code, and omp's
Rust core.

---

## 8. Caveats

- Performance numbers (`pi_agent_rust` <100ms/<50MB/~21.1 MiB; omp 61%/30% token
  savings) are **vendor self-reported**, not independently benchmarked —
  directional only.
- Star counts / activity are time-sensitive mid-2026 snapshots and only grow.
- The **opencode** name is ambiguous — original repo archived (Go, not Rust);
  maintained successors are Charm's Crush and sst/opencode.
- Iris's own current USP could not be independently sourced — it's a premise from
  the brief; the USP recommendation is editorial synthesis.
- **Thin coverage of major incumbents:** Claude Code, Codex CLI, Cursor/Cursor
  CLI, Gemini CLI, Cline, Hermes, and "Everything Claude Code 2" appeared only in
  passing or in the refuted claim. Coverage of the big-name field is weaker than
  the emerging Pi-lineage field.
- omp's implementation language had conflicting signals (README "all TypeScript"
  vs. "~55k lines of Rust core"); likely a Rust core with a TS surface —
  unresolved.

---

## 9. Open questions

1. Specific USPs/architecture/activity of the major named competitors not covered:
   Claude Code itself, Codex CLI (Rust), Gemini CLI, Cursor/Cursor CLI, Cline,
   Hermes, and "Everything Claude Code 2" built on Hermes.
2. Is omp a Rust core with a TS surface, or fully TS? Determines whether it's a
   direct Rust-native competitor.
3. Does Iris's "token caching" mean provider prompt-caching (Anthropic/OpenAI
   `cache-control`), and is anyone exposing it as a first-class harness feature,
   or is it commoditized like compaction?
4. Given `pi_agent_rust` already occupies the "authorized Rust port of Pi" niche
   with matching features, what concrete capability or workflow can Iris ship that
   the Pi/pi-mmr lineage and its Rust port do NOT have — i.e., the wedge?

---

## 10. Sources

**Primary (repos / docs / academic):**
- https://github.com/Aider-AI/aider · https://aider.chat/docs/repomap.html
- https://github.com/earendil-works/pi ·
  https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/README.md
- https://github.com/can1357/oh-my-pi
- https://github.com/Dicklesworthstone/pi_agent_rust · https://lib.rs/crates/pi_agent_rust
- https://github.com/Kuberwastaken/claurst · https://github.com/keon/mini-claude-code
- https://github.com/opencode-ai/opencode · https://github.com/charmbracelet/crush ·
  https://github.com/sst/opencode
- https://arxiv.org/pdf/2604.03515 ("Inside the Scaffold: A Source-Code Taxonomy
  of Coding Agent Architectures," Rombaut, Apr 2026, CC BY 4.0)
- pi-mmr repo (local; direct source reading): README, `docs/subagent-framework.md`,
  `docs/reference-architecture.md`, `docs/mmr-core-api.md`

**Secondary / blog:**
- https://github.com/bradAGI/awesome-cli-coding-agents
- https://mariozechner.at/posts/2025-11-30-pi-coding-agent/
- https://ampcode.com/manual
- https://dev.to/soulentheo/every-ai-coding-cli-in-2026-the-complete-map-30-tools-compared-4gob
- https://www.morphllm.com/ai-coding-agent
- https://harrisonsec.com/blog/claude-code-context-engineering-compression-pipeline/
- https://www.mindstudio.ai/blog/prompt-caching-claude-code-save-tokens
- https://codex.danielvaughan.com/2026/04/10/context-compaction-showdown-coding-agents/
- https://justin3go.com/en/posts/2026/04/09-context-compaction-in-codex-claude-code-and-opencode
- https://martinfowler.com/articles/harness-engineering.html
- https://www.firecrawl.dev/blog/what-is-an-agent-harness

**Run stats:** 6 angles · 23 sources fetched · 99 claims extracted · 25 verified ·
24 confirmed · 1 killed · 106 agent calls.
