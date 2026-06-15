# Iris Agent

Iris is a terminal-first coding agent being built in Rust. The product is split into:

- **Iris** — the coding agent and overall product.
- **Nexus** — the local agent runtime core.
- **Iris CLI** — the terminal interface.

## Current status

**Status (2026-06-15): early implementation.** The repository currently contains a text-only interactive REPL backed by an OpenAI Codex Responses provider, a provider tool-call loop, and workspace-scoped built-in tools. The Agent Kernel MVP is not complete yet: approval prompts, streaming terminal output, and session persistence are still planned.

Implemented today:

- CLI entrypoint in `src/main.rs`.
- Nexus REPL loop in `src/nexus.rs` with multi-turn conversation state, tool-call execution, and tool-result/error encoding.
- Provider-neutral `ChatProvider`, `AssistantTurn`, `ToolCall`, `Message`, and `Role` types.
- Workspace-scoped built-in tools in `src/tools.rs`: `read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`, and `hashline_edit`.
- OpenAI Codex OAuth browser and device-code login plus token loading/refresh in `src/auth/`.
- OpenAI Codex Responses request/response handling in `src/providers/openai_codex_responses.rs`, including tool schemas and streamed-response parsing.
- Unit tests for REPL behavior, tool loop behavior, workspace path safety, tool implementations, OAuth auth-file handling, URL resolution, request shaping, and response parsing.

Not implemented yet:

- Approval prompts and denial handling.
- Streaming output.
- Session persistence, modes, subagents, context ledger, content handles, git automation, and GitHub integration.

## Running

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
