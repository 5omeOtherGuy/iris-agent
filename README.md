# Iris Agent

Iris is a terminal-first coding agent being built in Rust. The product is split into:

- **Iris** — the coding agent and overall product.
- **Nexus** — the local agent runtime core.
- **Iris CLI** — the terminal interface.

## Current status

**Status (2026-06-14): early implementation.** The repository currently contains a text-only interactive REPL backed by an OpenAI Codex Responses provider. The Agent Kernel MVP is not complete yet: file tools, tool-call execution, approvals, workspace path safety, and bash execution are still planned.

Implemented today:

- CLI entrypoint in `src/main.rs`.
- Nexus REPL loop in `src/nexus.rs` with multi-turn conversation state.
- Provider-neutral `ChatProvider`, `Message`, and `Role` types.
- OpenAI Codex OAuth token loading/refresh in `src/auth/openai_codex.rs`.
- OpenAI Codex Responses request/response handling in `src/providers/openai_codex_responses.rs`.
- Unit tests for REPL behavior, OAuth auth-file handling, URL resolution, request shaping, and response parsing.

Not implemented yet:

- `read`, `write`, `edit`, and `bash` tools.
- Tool-call loop and tool-result encoding.
- Approval prompts and denial handling.
- Workspace path-safety enforcement.
- Streaming output.
- Session persistence, modes, subagents, context ledger, content handles, git automation, and GitHub integration.

## Running

Iris expects OpenAI Codex OAuth credentials in an Iris auth file. By default it reads:

```text
~/.iris/auth.json
```

Override the path with:

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
