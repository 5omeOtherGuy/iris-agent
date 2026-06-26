# Iris

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/hero-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="docs/assets/hero-light.svg">
  <img alt="Iris terminal banner. A user asks: What are you? The thinking indicator pulses, and the answer is: A precise, token-efficient coding agent for the terminal." src="docs/assets/hero-dark.svg" width="640">
</picture>

A fast coding agent for the terminal, built for token efficiency.

[Install](#install) · [Run](#run) · [The instrument](#the-instrument) · [Status](#status) · [Documentation](#documentation)

---

Iris is a coding agent you run in your terminal, for developers who already know
the change they want and read diffs faster than prose. Mutating tools require
explicit approval; nothing runs autonomously. It is written in Rust and ships as
a single binary.

For target users, product stance, and principles, see the [product brief](PRODUCT.md).

The tiers are mythology-named (see [Naming convention](docs/NAMING.md)):

- **Iris** — the coding agent, the overall product, and its terminal CLI (Tier 3).
- **Wayland** — the harness: sessions, config, and execution environment (Tier 2).
- **Nexus** — the local agent runtime core (Tier 1).
- **Mimir** — the provider package implementing Nexus's `ChatProvider` contract.

## Install

Install the latest version from the remote repository:

```bash
cargo install --git https://github.com/5omeOtherGuy/iris-agent.git --locked
```

Update an installed copy with:

```bash
iris update
```

Or run from a source checkout:

```bash
git clone https://github.com/5omeOtherGuy/iris-agent.git
cd iris-agent
cargo run
```

**Runtime dependencies: none beyond the binary.** The `grep` and `find` tools
search in-process via the ripgrep library crates (`grep`, `ignore`, `globset`),
so no `rg` or `fd` binary needs to be on `PATH`.

## Run

Create credentials for the provider you want, then start the REPL:

```bash
iris login openai-codex   # or: anthropic · antigravity
iris                      # /exit or /quit to leave
```

From a source checkout, replace `iris` with `cargo run --`.

At the prompt, `/model` views or switches provider/model and
`/reasoning off|minimal|low|medium|high|xhigh` changes thinking effort at a safe
turn boundary. `/settings`, `/scoped-models`, `/login`, and `/logout` open their
selectors.

<details>
<summary><b>Providers, settings &amp; environment</b></summary>

### Credentials and provider selection

Iris stores OAuth credentials in an Iris auth file. By default it reads
`~/.iris/auth.json`. Create or refresh credentials:

```bash
iris login openai-codex
iris login openai-codex --device-code
iris login anthropic
ANTIGRAVITY_CLIENT_SECRET=... iris login antigravity
```

- `openai-codex` uses OpenAI Codex OAuth (browser or device-code) and is the
  default provider if no setting is present.
- `anthropic` uses the Claude Code OAuth lane. `iris login anthropic` runs a
  browser PKCE login with a manual paste fallback; Iris can also bootstrap from
  Claude Code's token at `~/.claude/.credentials.json` (or
  `CLAUDE_CONFIG_DIR/.credentials.json`) when Anthropic credentials are not
  already in the Iris auth store.
- `antigravity` uses Google OAuth for Gemini Code Assist. Its installed-app
  client ID is public and decoded at runtime; the client secret is not committed
  to source and must be supplied via `ANTIGRAVITY_CLIENT_SECRET` at runtime or
  when building Iris.

Override the auth-file path with `IRIS_AUTH_PATH=/path/to/auth.json iris`.

### Settings

Choose the provider for a run with `defaultProvider` in the global JSON settings
file (`~/.iris/settings.json`, or `IRIS_CONFIG_PATH`):

```json
{
  "defaultProvider": "antigravity",
  "defaultModel": "gemini-3.5-flash"
}
```

Supported provider ids are `openai-codex`, `anthropic`, and `antigravity`.
Recognized settings keys are `defaultProvider`, `defaultModel`, `baseUrl`,
`contextTokenBudget`, `defaultReasoning`, `promptCacheRetention`,
`anthropicContextManagement`, and `enabledModels`.

Project settings (`<cwd>/.iris/settings.json`) are deliberately limited to
`defaultModel`, `defaultReasoning`, and `contextTokenBudget`; a cloned repo
cannot choose your provider, scoped model cycle, provider-side cache retention,
Anthropic server-side context-management behavior, or redirect OAuth bearer
tokens with `baseUrl`.

### Environment variables

- `IRIS_AUTH_PATH` — auth-file path; defaults to `~/.iris/auth.json`.
- `IRIS_MODEL` — OpenAI Codex model override; defaults to `gpt-5.5`.
- `IRIS_CODEX_BASE_URL` — OpenAI Codex base URL; defaults to `https://chatgpt.com/backend-api`.
- `IRIS_CONFIG_PATH` — global settings-file path; defaults to `~/.iris/settings.json`.
- `IRIS_SESSION_DIR` — session transcript root; defaults to `~/.iris/sessions`.
- `CLAUDE_CONFIG_DIR` — Claude Code config directory override for Anthropic token bootstrap.
- `ANTIGRAVITY_CLIENT_SECRET` — Antigravity Google OAuth client secret, read at runtime or embedded when set while building Iris; required for `login antigravity` and refresh unless the binary was built with it.
- `ANTIGRAVITY_PROJECT_ID` — optional Antigravity project-id override; when set it wins over any persisted project id, otherwise Iris discovers/persists one from `loadCodeAssist` and errors if discovery fails.

</details>

## The instrument

The interface is a single scrolling transcript with a fixed composer. Plain
language stays unboxed and light; only tool output and the composer earn chrome.
State is carried by a small, consistent symbol vocabulary — never by color alone,
fully legible in monochrome — so the surface stays calm at a glance and complete
on demand (`ctrl+o` reveals a panel's full output).

```text
  STATE        ◉ active mode    ● running         ◆ done · approved
               ◇ preview         ▲ review          ■ error · denied
               □ skipped         ○ queued · empty  ›  assistant turn

  PANELS       EXPLORE   read · grep · list · find
               SHELL     command execution
               EDIT      wrapped block diff  ( − removed · + added )
               APPROVAL  authorization review

  STATUSLINE   ◉ CODE ─ GPT-5.5 XHIGH ─ CTX 300K ●●●○○○○○○○
```

The banner above is rendered in this same language — the assistant marker `›`
and the LED working indicator. The full system lives in
[DESIGN.md](DESIGN.md) (token/format summary) and
[docs/TUI_DESIGN_LANGUAGE.md](docs/TUI_DESIGN_LANGUAGE.md) (the ground-truth pane
grammar). The banner SVGs are regenerated with
[`scripts/gen-hero-svg.py`](scripts/gen-hero-svg.py); a full session can be
recorded as an alternative via [`scripts/record-demo.sh`](scripts/record-demo.sh).

## Status

As of 2026-06-26: Milestone 1, the async-hard runtime, and the Milestone 2
foundations are complete. The next milestone gate is proving the token-efficiency
thesis with benchmark evidence; efficiency claims wait on measurement.

Implemented:

- Interactive terminal TUI, with a plain-text fallback for pipes and CI.
- Tokio async runtime with turn-level cancellation.
- Selectable Mimir providers (OpenAI Codex, Anthropic, Antigravity) with runtime model/reasoning switching.
- Workspace-scoped tools: read, write, edit, bash, grep, find, ls.
- Approval gates with diff previews for mutating tools.
- JSONL transcript persistence and linear resume.
- Large-output handles and turn-boundary auto-compaction.

Next:

- Token-efficiency benchmark proof.
- Persistent approval policies, in-session resume picker, transcript branching/rollback, modes, and subagents.

<details>
<summary><b>Implemented today</b></summary>

- CLI entrypoint in `src/main.rs` with typed-error exit codes (`src/errors.rs`) and `RUST_LOG` tracing setup (`src/telemetry.rs`).
- Iris CLI session loop in `src/cli.rs` driving the agent through a `Ui` front-end seam (`src/ui/`): the interactive TUI owns terminal-surface transcript replay, textarea input, slash/modals, live bash output cells, GFM-style streamed Markdown rendering, collapsed reasoning/thinking panels, capped-output preview/reveal, and word-level diff highlights; `src/ui/text.rs` remains the pipe/CI fallback.
- Nexus runtime core in `src/nexus.rs` (Tier 1): a tokio async loop with multi-turn conversation state, a per-turn `CancellationToken`, streamed assistant text via the async `ChatProvider::respond_stream` → `Stream<ProviderEvent>` contract, async `Tool::execute` (per-call child token, concurrency-safe tools batched in parallel), approval/diff-preview enforcement, and semantic `AgentEvent`/`UiEvent` rendering.
- Tier-2 `wayland::Harness` (`src/wayland/mod.rs`) wrapping the bare agent: owns the execution surface (workspace + `ToolState`), optional session log, output handle store, context budget, auto-compaction boundary, injects a `ToolEnv` per turn, and persists the transcript post-turn.
- Approval decision/parser in `src/approval.rs`, with `y`/`yes` allowing mutating tool calls and anything else denying safely.
- Provider-neutral `ChatProvider`, `AssistantTurn`, `ToolCall`, `Message`, and `Role` types.
- Runtime `/model` and `/reasoning` switching at safe turn boundaries, plus TUI pickers for model/provider/effort, scoped model cycling, `/settings`, `/login`, and `/logout`.
- Default-off provider-native prompt-cache controls (`promptCacheRetention: "none" | "short" | "long"`), stable-prefix cache-break diagnostics, provider usage/cache accounting, and Anthropic-only `anthropicContextManagement` clear-edit opt-ins; Anthropic provider-side compact is intentionally rejected until compaction blocks can be persisted and replayed.
- Workspace-scoped built-in tools under `src/tools/`: `read`, `write`, `edit`, `bash`, `grep`, `find`, and `ls`, built and injected via `tools::built_in_tools()` (`src/tools/registry.rs`) as `Tool` trait impls that self-classify (`requires_approval`/`is_destructive`/`is_concurrency_safe`/`diff_preview`). `edit` uses Claude Code's exact-string contract (`file_path`/`old_string`/`new_string`/`replace_all`). The harness no longer imposes a default tool-roundtrip cap or default bash timeout; real safety rails remain around workspace access, approval, process cleanup, capture memory, and large file/output handling.
- Approval enforcement for `write`, `edit`, and `bash`, with diff previews for file-mutating tools and model-readable denied-call results.
- Atomic same-directory file replacement for `write` and `edit`.
- `bash` hardening: a Linux Landlock kernel sandbox (workspace-write, TCP-deny) with explicit fallback, persistent shell sessions, and background jobs (`src/tools/bash/`).
- Graceful Ctrl-C handling: first press ends the turn between round-trips, a second force-quits and reaps tracked process groups (`src/signals.rs`, `src/process_group.rs`).
- A JSON settings file for provider/model/reasoning/context defaults and scoped model cycling (`src/config.rs`, `~/.iris/settings.json` + project `.iris/settings.json`).
- Best-effort JSONL session transcripts, linear `iris resume <id>`, model-selection audit entries, assistant-reasoning rows, compaction entries, and token estimates (`src/session.rs`).
- Session-scoped large-output handle storage (`src/handles.rs`) and handle-backed tool results when output exceeds the context threshold.
- TUI implementation seams for current and future rendering work: `Component`/`Container` composition, explicit overlay focus routing, a shared Unicode/ANSI text engine, and a built-in `ToolRenderer` registry with safe generic fallback for unknown tools.
- Harness-owned system prompt assembly from materialized global fragments, repo fragments, project docs, runtime context, and the live tool registry (`src/wayland/system_prompt/`).
- Mimir auth flows and token loading/refresh under `src/mimir/auth/`: shared cancellable loopback OAuth callback plumbing, OpenAI Codex browser/device-code OAuth, Anthropic Claude Code browser OAuth and credential reuse, and Antigravity Google PKCE OAuth (with runtime or build-time client-secret injection).
- Mimir providers under `src/mimir/providers/`: OpenAI Codex Responses, Anthropic Messages (Claude Code subscription lane), and Antigravity/Gemini Code Assist streaming, all translated into Nexus's `ChatProvider` contract with normalized reasoning controls where supported. Anthropic signed/redacted thinking and Gemini `thoughtSignature` tool-call continuity round-trip through provider-neutral opaque continuity fields.
- Unit tests for session/loop behavior, streaming, approval allow/deny paths, diff-preview ordering, workspace path safety, typed-error classification, telemetry redaction, tool implementations, OAuth auth-file handling, runtime selection, prompt assembly, URL resolution, request shaping, and response parsing.

</details>

<details>
<summary><b>Not implemented yet</b></summary>

- Persistent approval policies, in-session `/resume` picker, transcript-tree branching/rollback, modes, subagents, context ledger/planner, handle dereference UI/tool, token-efficiency benchmark proof, git automation, and GitHub integration.
- A possible plugin system for third-party extensions (WASM/Extism is one candidate backend, a subprocess protocol another) — exploratory only, tracked in issue #18; Iris is not being built around it.

</details>

## Testing

```bash
cargo test
```

## Documentation

- [Naming convention](docs/NAMING.md) — how the Iris/Wayland/Nexus/Mimir tiers are named.
- [Roadmap](docs/ROADMAP.md) — milestone sequencing and acceptance gates.
- [Feature list](docs/FEATURES.md) — implemented/planned capability inventory.
- [Product brief](PRODUCT.md) — target users, product purpose, voice, and product principles.
- [Design system summary](DESIGN.md) — concise visual-system summary for the Iris TUI.
- [Current codemap](docs/CODEMAPS/INDEX.md) — source-grounded map of the current codebase.
- [TUI design language](docs/TUI_DESIGN_LANGUAGE.md) — terminal layout, spacing, and menu rules.
- [TUI live testing](docs/TUI_LIVE_TESTING.md) — opt-in tmux harness for manual pane-rendering checks.
- [Architecture Decision Records](docs/adr/README.md) — accepted/proposed architecture decisions.
- [Competitor matrix](docs/COMPETITOR_MATRIX.md) — verified competitor feature matrix.
- [Competitor analysis](docs/COMPETITOR_ANALYSIS.md) — strategic competitor notes.

## License

[MIT](LICENSE).
