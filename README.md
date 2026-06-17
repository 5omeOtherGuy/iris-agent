# Iris Agent

```
╭───────────────────────────────╮
│                               │
│   ██       ██                 │
│   ██ ██▄▄▖ ██ ▄████           │
│   ██ ███▀▘ ██ ▀▀▀██           │
│   ██ ██    ██ ▄▄▄██           │
│   ██ ██    ██ █████           │
│                               │
│   "I'd ship this one!"        │
│        — Claude Code, 2026    │
│                               │
╰───────────────────────────────╯
```

Iris is a terminal-first coding agent being built in Rust. The product is split into mythology-named tiers (see [Naming convention](docs/NAMING.md)):

- **Iris** — the coding agent and overall product, and its terminal CLI (Tier 3).
- **Wayland** — the harness that equips the core with sessions/config/execution env (Tier 2).
- **Nexus** — the local agent runtime core (Tier 1).
- **Mimir** — the AI/provider package implementing Nexus's `ChatProvider` contract.

## Current status

**Status (2026-06-17): Milestone 1 and the async-hard runtime are complete.** The repository currently contains a text-only interactive session backed by an OpenAI Codex Responses provider, streamed response parsing, workspace-scoped built-in tools, terminal approval gates with diff previews, provider/model settings, and best-effort JSONL transcript persistence. Nexus now runs a tokio async loop with turn-level cancellation: the provider is an async event stream raced against cancellation, tools are async with per-call child tokens, concurrency-safe tools run in parallel while everything else stays exclusive, and the transcript stays valid on abort. The next runtime work is Milestone 2 (token/context).

Implemented today:

- CLI entrypoint in `src/main.rs` with typed-error exit codes (`src/errors.rs`) and `RUST_LOG` tracing setup (`src/telemetry.rs`).
- Iris CLI session loop in `src/cli.rs` driving the agent through a `Ui` front-end seam (`src/ui/`, text implementation in `src/ui/text.rs`).
- Nexus runtime core in `src/nexus.rs` (Tier 1): a tokio async loop with multi-turn conversation state, a per-turn `CancellationToken`, streamed assistant text via the async `ChatProvider::respond_stream` → `Stream<ProviderEvent>` contract, async `Tool::execute` (per-call child token, concurrency-safe tools batched in parallel), approval/diff-preview enforcement, and semantic `AgentEvent`/`UiEvent` rendering.
- Tier-2 `wayland::Harness` (`src/wayland.rs`) wrapping the bare agent: owns the execution surface (workspace + `ToolState`) and optional session log, injects a `ToolEnv` per turn, and persists the transcript post-turn.
- Approval decision/parser in `src/approval.rs`, with `y`/`yes` allowing mutating tool calls and anything else denying safely.
- Provider-neutral `ChatProvider`, `AssistantTurn`, `ToolCall`, `Message`, and `Role` types.
- Workspace-scoped built-in tools under `src/tools/`: `read`, `write`, `edit`, `bash`, `grep`, `find`, and `ls`, built and injected via `tools::built_in_tools()` (`src/tools/registry.rs`) as `Tool` trait impls that self-classify (`requires_approval`/`is_destructive`/`is_concurrency_safe`/`diff_preview`). `edit` uses Claude Code's exact-string contract (`file_path`/`old_string`/`new_string`/`replace_all`).
- Approval enforcement for `write`, `edit`, and `bash`, with diff previews for file-mutating tools and model-readable denied-call results.
- Atomic same-directory file replacement for `write` and `edit`.
- `bash` hardening: a Linux Landlock kernel sandbox (workspace-write, TCP-deny) with explicit fallback, persistent shell sessions, and background jobs (`src/tools/bash/`).
- Graceful Ctrl-C handling: first press ends the turn between round-trips, a second force-quits and reaps tracked process groups (`src/signals.rs`, `src/process_group.rs`).
- A JSON settings file for provider/model defaults (`src/config.rs`, `~/.iris/settings.json` + project `.iris/settings.json`).
- Best-effort JSONL session transcripts (`src/session.rs`).
- OpenAI Codex OAuth browser and device-code login plus token loading/refresh in the Mimir provider package (`src/mimir/auth/`).
- OpenAI Codex Responses request/response handling in `src/mimir/providers/openai_codex_responses.rs`, including tool schemas, retry/backoff, and streamed-response parsing.
- Unit tests for session/loop behavior, streaming, approval allow/deny paths, diff-preview ordering, workspace path safety, typed-error classification, telemetry redaction, tool implementations, OAuth auth-file handling, URL resolution, request shaping, and response parsing.

Not implemented yet:

- Persistent approval policies, session `/resume` and transcript-tree branching, modes, subagents, context ledger, content handles, git automation, and GitHub integration.
- A possible plugin system for third-party extensions (WASM/Extism is one candidate backend, a subprocess protocol another) — exploratory only, tracked in issue #18; Iris is not being built around it.

## Running

### Runtime dependencies

None beyond the binary itself. The `grep` and `find` tools search in-process via
the ripgrep library crates (`grep`, `ignore`, `globset`), so no `rg` or `fd`
binary needs to be on `PATH`.

### Credentials

Iris expects OpenAI Codex OAuth credentials in an Iris auth file. By default it reads:

```text
~/.iris/auth.json
```

Create or refresh credentials with one of the login commands:

```bash
cargo run -- login openai-codex
cargo run -- login openai-codex --device-code
```

Override the auth-file path with:

```bash
IRIS_AUTH_PATH=/path/to/auth.json cargo run
```

Provider/model defaults can also be set in a JSON settings file: `~/.iris/settings.json`
(global) and `<cwd>/.iris/settings.json` (project, overrides global). Recognized keys:
`defaultProvider`, `defaultModel`, `baseUrl`. Environment variables override the file.

Optional environment variables:

- `IRIS_MODEL` — model name; defaults to `gpt-5.5`.
- `IRIS_CODEX_BASE_URL` — base URL; defaults to `https://chatgpt.com/backend-api`.
- `IRIS_CONFIG_PATH` — global settings-file path; defaults to `~/.iris/settings.json`.
- `IRIS_SESSION_DIR` — session transcript root; defaults to `~/.iris/sessions`.

Start the REPL:

```bash
cargo run
```

Exit with `/exit` or `/quit`.

## Testing

Run the current test suite with:

```bash
cargo test
```

## Documentation

- [Naming convention](docs/NAMING.md) — how the Iris/Wayland/Nexus/Mimir tiers are named.
- [Roadmap](docs/ROADMAP.md) — milestone sequencing and acceptance gates.
- [Feature list](docs/FEATURES.md) — implemented/planned capability inventory.
- [Pitch](docs/PITCH.md) — product direction and positioning.
- [Current codemap](docs/CODEMAPS/INDEX.md) — source-grounded map of the current codebase.
- [Competitor matrix](docs/COMPETITOR_MATRIX.md) — verified competitor feature matrix.
- [Competitor analysis](docs/COMPETITOR_ANALYSIS.md) — strategic competitor notes.
