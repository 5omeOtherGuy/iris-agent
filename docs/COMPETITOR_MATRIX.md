# Competitor Feature Matrix (verified)

> **Method.** Primary-source verification pass (GitHub repos/API, npm/PyPI
> registries, official LICENSE files & docs). Cells are either primary-sourced or
> explicitly marked `unverified` — nothing is filled from general impression.
> **Date checked: 2026-06-14.** Every figure is a point-in-time snapshot;
> re-verify before external use.
>
> **Coverage:** 12 direct competitors verified, plus 1 requested row
> (**Everything Claude Code / ECC**) confirmed as a **plugin/rules pack, not a
> standalone coding-agent competitor**. Agent-mechanics cells remain `unverified`
> where the primary source did not make the claim directly.
>
> † `pi-mmr` is the Iris author's own predecessor project — a conflict of
> interest; included for completeness, not as an independent competitor.

## Table A — Identity & maturity

| Competitor | Language / single binary | License | Stars | Latest / status | Checked |
|---|---|---|---|---|---|
| **OpenAI Codex CLI** | Rust 96.1% / **native binary** (going native, zero-dep goal) | unverified | unverified | `rust-v0.140.0-alpha.19` (pre-release); stable `0.139.0` (2026-06-09); **pre-1.0**, daily alpha cadence | 2026-06-14 |
| **Claude Code** | npm package / **no** (Node ≥18; `claude` bin) | SEE LICENSE IN README.md | unverified | npm `@anthropic-ai/claude-code` v2.1.177; proprietary Anthropic product | 2026-06-14 |
| **Cursor / Cursor CLI** | Proprietary editor + CLI / **yes-ish** (Cursor app/CLI install; implementation unverified) | proprietary | unverified | active product; CLI installed via `curl https://cursor.com/install -fsS \| bash` in official CI examples | 2026-06-14 |
| **Google Gemini CLI** | TypeScript 98% / **no** (needs Node ≥20) | Apache-2.0 | ~105.3k | `v0.46.0` (2026-06-10), 523 releases | 2026-06-14 |
| **Cline** | TypeScript/JS monorepo / **no** (Bun/Node; VS Code extension, CLI, SDK) | Apache-2.0 (`@cline/cli`) | unverified | npm `@cline/cli` v0.0.13 experimental; repo docs list SDK, CLI, VS Code, JetBrains, Kanban | 2026-06-14 |
| **Aider** | Python / **no** (Python ≥3.10,<3.13 via PyPI/uv/pipx) | unverified | unverified | PyPI `aider-chat` v0.86.2; terminal pair programmer | 2026-06-14 |
| **Hermes Agent** | Python / **no** (installer provisions Python 3.11, uv, Node, ripgrep, ffmpeg, Git Bash) | MIT | ~193k (GitHub UI snapshot) | `hermes-agent` v0.16.0 in `pyproject.toml`; active NousResearch agent | 2026-06-14 |
| **pi_agent_rust** | Rust / **yes** (static, no Node/Bun) | dual "MIT + Rider" (GitHub: NOASSERTION) | ~1,170 | `v0.1.18` (tag v0.1.20), **pre-1.0, single-author** | 2026-06-14 |
| **pi-mmr** † | TS/JS / **no** (Pi extension, needs Pi host) | MIT | 1 | `v0.2.0` (2026-06-05), **16-day-old, single-author** | 2026-06-14 |
| **opencode-ai/opencode** | Go 99.2% / yes | MIT | unverified | **ARCHIVED 2025-09-18** (read-only) | 2026-06-14 |
| **Charm Crush** | Go 98.4% / yes | **FSL-1.1-MIT** (source-available, →MIT after 2yr) | ~25.3k | `v0.77.0` (2026-06-14), 165 releases | 2026-06-14 |
| **sst/opencode** | TypeScript 68.8% / yes (per-platform prebuilt) | MIT | unverified | npm `opencode-ai` v1.17.7 | 2026-06-14 |

## Table B — Capabilities (verified cells only)

| Competitor | Providers | Compaction | Prompt caching | Modes / multi-model routing | Subagents (exec model) | Repo map | Git / GitHub | MCP | Edit format |
|---|---|---|---|---|---|---|---|---|---|
| **Codex CLI** | unverified | unverified | unverified | unverified | unverified (docs exist) | unverified | unverified | unverified | unverified |
| **Claude Code** | Anthropic + third-party integrations for Terminal/VS Code | **yes** (auto-compaction near context limit; `/compact` custom instructions) | **yes** (automatic prompt caching; Vertex/Bedrock env vars can disable/request 1h TTL) | model selection via `--model`; plan mode documented | **yes** — built-in and custom subagents; project/user/plugin/CLI-defined; subagent frontmatter supports model/tools/MCP/background/isolation | code intelligence plugins; repo map unverified | **yes** — GitHub Actions, PR/issue automation, code review | **yes** — stdio/SSE/HTTP/WebSocket MCP servers, `.mcp.json`, plugins | unverified |
| **Cursor / Cursor CLI** | Cursor model menu; concrete provider list unverified | unverified | unverified | Agent mode + Plan Mode; normal/Agent mode persistence | Background Agents documented; subagent exec model unverified | **yes** — Codebase Indexing docs | **yes** — GitHub/Git integrations and CI examples | **yes** — MCP docs and CLI MCP pages | unverified |
| **Gemini CLI** | **Google only** (single-provider) | unverified | unverified | unverified | unverified | unverified | unverified | unverified | unverified |
| **Cline** | BYOK/provider selection + local models (docs list Cloud Providers / Running Models Locally) | **yes** (Auto Compact summarizes and replaces conversation history) | **yes-ish** (Auto Compact says summarization leverages the existing prompt cache) | **Plan / Act**; optional different models per Plan vs Act | **yes** — read-only research subagents launched simultaneously; enabled by default | project-structure understanding; explicit repo-map unverified | **yes** — checkpoints via shadow Git repo; worktrees docs; review/rollback workflow | **yes** — Marketplace/manual MCP; CLI uses `cline mcp` | diff review in IDE; exact edit protocol unverified |
| **Aider** | **broad** — OpenAI, Anthropic, Gemini, Groq, LM Studio, xAI, Azure, Cohere, DeepSeek, Ollama, OpenAI-compatible, OpenRouter, Copilot, Vertex, Bedrock, others | unverified | **yes** (`--cache-prompts`; Anthropic Sonnet/Haiku + DeepSeek Chat; cache keepalive pings) | **code / ask / architect / help**; architect uses separate editor model | none documented | **yes** — concise whole-repo map with graph ranking and token budget | **yes** — auto-commits edits, dirty-file commits, `/undo`, commit attribution | unverified | **whole, diff, diff-fenced, udiff, editor-diff, editor-whole** |
| **Hermes Agent** | OpenAI dependency; provider configuration docs exist; exact provider list unverified | unverified | unverified | personalities/sessions documented; routing unverified | Skills system; subagent model unverified | unverified | Git Bash bundled/used for shell commands; Git workflow features unverified | **yes** — MCP Integration docs and optional MCP package data | unverified |
| **pi_agent_rust** | **10 native** + OpenAI-compat presets | **yes** (summarize + preserve-recent; trigger `ctx_window − reserve`) | unverified | model switching; **no** mode profiles | **none documented** | **no** | **none** | **no** native MCP | **content-hash anchored** (`hashline_edit`, LINE#HASH) + line |
| **pi-mmr** † | provider-neutral (inherits Pi auth) | inherits Pi (unverified) | unverified | **yes — 5 locked modes** + free | **yes** — finder/oracle/librarian/Task + custom `sa__*`; **worker (child) process** | no (search via `finder`) | GitHub read-only (`librarian`) | planned (no) | structured patch (`apply_patch`) |
| **opencode-ai** (archived) | 9 (frozen): Anthropic, OpenAI, Gemini/Vertex, Bedrock, Groq, Azure, OpenRouter, Copilot (exp), OpenAI-compat | **yes** (~95% ctx, summarize + replace) | unverified | **per-agent model** selection | per-agent (config) | unverified | unverified | unverified | unverified |
| **Crush** | **wide** (catwalk registry) + OpenAI/Anthropic-compat | unverified | unverified | unverified | unverified | unverified | unverified | unverified | unverified |
| **sst/opencode** | 75+ (reported, not re-verified) | unverified | unverified | "agents" (docs exist) | unverified | unverified | unverified | unverified | unverified |

## Requested row dropped from direct-competitor set

| Row | Verified disposition | Why |
|---|---|---|
| **Everything Claude Code / ECC** (`affaan-m/everything-claude-code`, now branded **ECC**) | **Drop as direct competitor; track as an ecosystem/plugin pack.** | README/package describe a "harness-native agent operating system" and Claude Code plugin/skills/rules/hooks pack for Claude Code, Codex, OpenCode, Cursor, Gemini and terminal workflows. It is MIT, npm `ecc-universal` v2.0.0, Node ≥18, with 64 specialized subagent definitions, skills, commands, hooks, MCP configs, and install scripts — not a standalone coding-agent runtime. |

## Closest direct analogues to Iris
*(Rust, single-binary, token-efficiency-focused, mode + subagent routing — medium-confidence positioning synthesis.)*

- **`pi_agent_rust`** — closest on the **implementation/distribution axis**: Rust +
  single static binary + no Node/Bun + token-efficiency framing + 10-provider
  multi-model routing + content-hash anchored edits. *Early-stage, single-author,
  pre-1.0.*
- **`pi-mmr`** — closest on the **routing/orchestration axis**: 5-mode multi-model
  routing + subagent/worker fleet with background execution. *The author's own
  16-day-old, 1-star project.*
- **Codex CLI** — closest **mature** Rust single-binary harness and the primary
  Rust reference for Iris's next async-runtime work, but single-vendor (OpenAI)
  and not token-efficiency-positioned.
- **Crush / sst/opencode / Hermes** — closest **mature multi-provider or
  tool-rich harnesses**, but Go, TypeScript, and Python respectively, not Rust.
- **Claude Code / Cline / Cursor** — strongest incumbents on UX, ecosystem, and
  agent features; none are Rust single-binary direct analogues.

## Strategic takeaways for Iris
1. **Rust + single binary is table-stakes, now even at the incumbent tier.** Codex
   CLI is 96% Rust and explicitly "going native" (zero-dependency binary). Do not
   sell "Rust."
   For implementation, still study Codex: its mature Tokio stream/cancellation
   runtime is a better Nexus reference than `pi_agent_rust`'s bespoke runtime.
2. **Content-hash anchored edits are not a differentiator** — `pi_agent_rust`
   (the closest competitor) already ships `hashline_edit`. Treat as parity, not
   novelty.
3. **First-class provider prompt-caching is not open whitespace.** Claude Code and
   Aider both document prompt caching; Cline documents use of an existing prompt
   cache during Auto Compact. Iris can still differentiate on cache controls,
   transparency, or cross-provider implementation, but not on the existence of
   prompt caching.
4. **Nested subagents are common, but execution models differ.** Claude Code and
   Cline document subagents; `pi-mmr` uses child/worker processes; Cline subagents
   are read-only research agents. The "in-process nested subagent" angle remains
   plausibly distinctive, but only against the verified execution models above.
5. **The two closest Rust/routing analogues are both very young single-author
   projects.** Mature direct competition is stronger on UX/ecosystem than on the
   exact Rust + routing axis.

## Gaps — remaining unverified cells (do NOT fill from impression)
- Several agent-mechanics cells remain `unverified` because primary docs were not
  located in this pass, especially exact edit protocols, repo-map implementations,
  and prompt-caching details for Cursor, Hermes, Crush, sst/opencode, Codex, and
  the Pi rows.
- Stars for some proprietary or rate-limited GitHub/API targets remain
  `unverified`; re-check with authenticated GitHub API before publication.


## 2026-07 local addendum — Grok Build CLI

| Competitor | Evidence | Subagents | Isolation / apply | Worktree management | Iris relevance |
|---|---|---|---|---|---|
| **Grok Build CLI 0.2.82** | Local CLI/runtime reference pass; see [`.iris-reference/grok-worktree-subsystem-spec.md`](../.iris-reference/grok-worktree-subsystem-spec.md). | **yes** — model-facing input includes prompt, description, subagent type, background execution, capability mode, isolation, resume, and cwd controls; background tasks emit subagent/session notifications; depth is limited. | **best verified** — worktree isolation runs child mutations in a separate worktree; parent/source workspace changes only through explicit apply; best-of-N applies one winner. | **mature** — worktree list/show/rm/gc/db-style management; durable registry with worktree id, source repo, session id, creation mode, owner pid, status; dead-process cleanup, GC, and fast snapshot/copy/git-checkout fallbacks. | Treat as the target systems design for Iris mutable subagents. Copy semantics first: linked worktree, registry, explicit apply, progress, list/show/rm/gc. Defer Btrfs/overlay fast paths until linked semantics are correct. |

## Caveats
- **Licensing nuance:** Crush is **FSL-1.1-MIT** (source-available, prohibits
  "Competing Use," converts to MIT after 2 years — *not* OSI-permissive at release);
  `pi_agent_rust` is dual "MIT + Rider" (GitHub reports NOASSERTION); Gemini CLI is
  Apache-2.0 but the backing service has separate Google ToS; Claude Code is an npm
  package with `SEE LICENSE IN README.md`; Cursor is proprietary.
- **Archival/successor split:** `opencode-ai/opencode` is archived (2025-09-18) and
  split into two contested successors — `charmbracelet/crush` (Charm) and
  `sst/opencode` (Dax/Adam). Different languages and provider sets; do not conflate.
- **Conflict of interest:** `pi-mmr` is the Iris author's own repo — verified
  against source, but self-interested and immature.
- **Time-sensitivity:** Codex ships multiple alphas/day; Gemini CLI had a same-day
  push and an *unconfirmed* third-party report of a 2026-06-18 rename to "Antigravity
  CLI"; Crush ships ~12 releases/month via the evolving catwalk registry; Claude
  Code and Cursor are proprietary products with fast-moving docs and versioning.

## Sources (primary unless noted)
- Codex CLI: `github.com/openai/codex`, `/releases`, `/discussions/1174`
- Claude Code: `docs.anthropic.com/en/docs/claude-code/overview`, `/sub-agents`,
  `/mcp`, `/memory`, `/github-actions`, `/costs`; npm `@anthropic-ai/claude-code`
- Cursor: `docs.cursor.com/en/cli/installation`, `/en/cli/cookbook/fix-ci`,
  `/en/agent/modes`, `/en/context/codebase-indexing`, `/mcp`, `/en/account/agent-security`,
  `/docs/integrations/github` (reader could not statically extract all pages; search
  snippets and static HTML metadata were used where noted)
- Gemini CLI: `github.com/google-gemini/gemini-cli`, GitHub API, npm `@google/gemini-cli`
- Cline: `github.com/cline/cline` README/package, npm `@cline/cli`, `docs.cline.bot`
  (`/features/plan-and-act`, `/features/auto-compact`, `/features/subagents`,
  `/core-workflows/checkpoints`, `/mcp/mcp-overview`)
- Aider: `aider.chat/docs` (`/install.html`, `/llms.html`, `/usage/modes.html`,
  `/repomap.html`, `/git.html`, `/more/edit-formats.html`, `/usage/caching.html`),
  PyPI `aider-chat`
- Hermes Agent: `github.com/NousResearch/hermes-agent` README, `pyproject.toml`,
  docs index links under `hermes-agent.nousresearch.com/docs/...`
- pi_agent_rust: `github.com/Dicklesworthstone/pi_agent_rust` (+ `/tree/main/src/providers`), GitHub API
- pi-mmr: `github.com/5omeOtherGuy/pi-mmr`, GitHub API, local source
- opencode-ai: `github.com/opencode-ai/opencode` (+ LICENSE), DeepWiki (secondary)
- Crush: `github.com/charmbracelet/crush`, GitHub API, SPDX FSL-1.1-MIT, repo Discussion #1482
- sst/opencode: `github.com/sst/opencode`, npm `opencode-ai`
- Everything Claude Code / ECC: `github.com/affaan-m/everything-claude-code` README,
  `package.json`; npm `ecc-universal`

*Run stats: targeted second pass closed the 6 previously unverified requested rows: 5 direct competitors added/verified; 1 row (ECC) dropped as non-direct competitor.*
