# Iris Agent

Iris is a terminal-first coding agent being built in Rust. The product is split into:

- **Iris** — the coding agent and overall product.
- **Nexus** — the local agent runtime core.
- **Iris CLI** — the terminal interface.

## Current status

**Status (2026-06-15): early implementation.** The repository currently contains a text-only interactive session backed by an OpenAI Codex Responses provider, a streaming provider tool-call loop, workspace-scoped built-in tools, and terminal approval gates with diff previews. The Agent Kernel MVP is close but not complete yet: session persistence and broader tool-policy UX are still planned.

Implemented today:

- CLI entrypoint in `src/main.rs` with typed-error exit codes (`src/errors.rs`) and `RUST_LOG` tracing setup (`src/telemetry.rs`).
- Iris CLI session loop in `src/cli.rs` driving the agent through a `Ui` front-end seam (`src/ui/`, text implementation in `src/ui/text.rs`).
- Nexus runtime in `src/nexus.rs` with multi-turn conversation state, streamed assistant text via `TurnSink`, tool-call execution, approval/diff-preview enforcement, and semantic `UiEvent` rendering.
- Approval decision/parser in `src/approval.rs`, with `y`/`yes` allowing mutating tool calls and anything else denying safely.
- Provider-neutral `ChatProvider`, `TurnSink`, `AssistantTurn`, `ToolCall`, `Message`, and `Role` types.
- Workspace-scoped built-in tools under `src/tools/`: `read`, `write`, `edit`, `bash`, `grep`, `find`, and `ls`. `edit` uses Claude Code's exact-string contract (`file_path`/`old_string`/`new_string`/`replace_all`).
- Approval enforcement for `write`, `edit`, and `bash`, with diff previews for file-mutating tools and model-readable denied-call results.
- Atomic same-directory file replacement for `write` and `edit`.
- OpenAI Codex OAuth browser and device-code login plus token loading/refresh in `src/auth/`.
- OpenAI Codex Responses request/response handling in `src/providers/openai_codex_responses.rs`, including tool schemas, retry/backoff, and streamed-response parsing.
- Unit tests for session/loop behavior, streaming, approval allow/deny paths, diff-preview ordering, workspace path safety, typed-error classification, telemetry redaction, tool implementations, OAuth auth-file handling, URL resolution, request shaping, and response parsing.

Not implemented yet:

- Persisted session transcripts.
- Persistent approval policies, shared file-observation/stale-file guards, modes, subagents, context ledger, content handles, git automation, and GitHub integration.

## Running

### Runtime dependencies

The `grep` and `find` tools shell out to external search binaries, which must be
on `PATH`:

- [`ripgrep`](https://github.com/BurntSushi/ripgrep) (`rg`) for `grep`.
- [`fd`](https://github.com/sharkdp/fd) (`fd` or `fdfind`) for `find`.

If one is missing, only that tool fails with a clear message; the rest of the
agent works normally.

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

Optional environment variables:

- `IRIS_MODEL` — model name; defaults to `gpt-5.5`.
- `IRIS_CODEX_BASE_URL` — base URL; defaults to `https://chatgpt.com/backend-api`.

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

- [Roadmap](docs/ROADMAP.md) — milestone sequencing and acceptance gates.
- [Feature list](docs/FEATURES.md) — implemented/planned capability inventory.
- [Pitch](docs/PITCH.md) — product direction and positioning.
- [Current codemap](docs/CODEMAPS/INDEX.md) — source-grounded map of the current codebase.
- [Competitor matrix](docs/COMPETITOR_MATRIX.md) — verified competitor feature matrix.
- [Competitor analysis](docs/COMPETITOR_ANALYSIS.md) — strategic competitor notes.
