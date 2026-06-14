# Iris Current Codemap

**Last Updated:** 2026-06-14  
**Entry Points:** `src/main.rs`

This codemap describes implemented code only. Planned capabilities live in [`../ROADMAP.md`](../ROADMAP.md) and [`../FEATURES.md`](../FEATURES.md).

## Architecture

╭──────────╮     ╭──────────────╮     ╭────────────────────────────╮
│ Iris CLI │────▶│ Nexus Agent  │────▶│ OpenAI Codex Responses     │
│ main.rs  │     │ nexus.rs     │     │ provider                   │
╰──────────╯     ╰──────┬───────╯     ╰─────────────┬──────────────╯
                        │                           │
                        │                           ▼
                        │              ╭────────────────────────────╮
                        ╰─────────────▶│ OpenAI Codex OAuth auth    │
                                       │ token store / refresh      │
                                       ╰────────────────────────────╯

## Key Modules

| Module | Purpose | Public/internal API | Dependencies |
|---|---|---|---|
| `src/main.rs` | CLI entrypoint. Builds the provider from environment and starts the agent loop. | `main()` | `nexus::Agent`, `OpenAiCodexResponsesProvider` |
| `src/nexus.rs` | Runtime core for the current text-only REPL. Maintains conversation state and calls a provider. | `ChatProvider`, `Agent`, `Message`, `Role` | `std::io`, `anyhow` |
| `src/auth/mod.rs` | Auth module declaration. | `openai_codex` module | `src/auth/openai_codex.rs` |
| `src/auth/openai_codex.rs` | Reads OpenAI Codex OAuth credentials, refreshes expired access tokens, extracts account ID from JWT payload, writes refreshed auth atomically. | `OpenAiCodexTokenStore`, `AccessToken` | `anyhow`, `base64`, `reqwest`, `serde`, `serde_json`, filesystem/env APIs |
| `src/providers/mod.rs` | Provider module declaration. | `openai_codex_responses` module | `src/providers/openai_codex_responses.rs` |
| `src/providers/openai_codex_responses.rs` | Implements `ChatProvider` for the ChatGPT Codex Responses endpoint. Builds request JSON, headers, URL, and parses assistant text. | `OpenAiCodexResponsesProvider` | `OpenAiCodexTokenStore`, `ChatProvider`, `reqwest`, `serde_json` |

## Data Flow

1. `main()` creates `OpenAiCodexResponsesProvider::from_env()`.
2. `Agent::run()` starts a REPL and delegates to `run_with()`.
3. User prompts are appended as `Message { role: User, content }`.
4. `ChatProvider::respond()` receives the full in-memory message list.
5. The OpenAI Codex provider reads or refreshes OAuth credentials.
6. The provider converts Nexus messages into Codex Responses request JSON.
7. The provider sends a blocking HTTP request and extracts assistant text.
8. Nexus prints `assistant> ...` and appends the assistant response to conversation state.
9. Provider errors are printed to stderr and the REPL continues.

## Configuration and Inputs

| Input | Default | Used by |
|---|---|---|
| `IRIS_AUTH_PATH` | `~/.iris/auth.json` | `OpenAiCodexTokenStore::from_env()` |
| `IRIS_MODEL` | `gpt-5.5` | `OpenAiCodexResponsesConfig::from_env()` |
| `IRIS_CODEX_BASE_URL` | `https://chatgpt.com/backend-api` | `OpenAiCodexResponsesConfig::from_env()` |
| `HOME` | required when `IRIS_AUTH_PATH` is unset | auth path resolution |

## External Dependencies

- `anyhow` — error propagation and context.
- `base64` — base64url JWT payload decoding.
- `reqwest` — blocking HTTP client with JSON and rustls TLS.
- `serde` — auth-file serialization/deserialization.
- `serde_json` — JSON request/response construction and parsing.

## Tests

Current unit tests cover:

- REPL conversation persistence and provider-error recovery in `src/nexus.rs`.
- JWT account extraction, auth-file parsing, malformed auth errors, and atomic restricted auth writes in `src/auth/openai_codex.rs`.
- Codex URL resolution, request JSON construction, response text extraction, and missing-text errors in `src/providers/openai_codex_responses.rs`.

## Known Gaps

The Agent Kernel MVP is not complete. Missing runtime areas include core tools, tool-call execution, tool-result encoding, approval policy, workspace path safety, bash policy, streaming, and session persistence.

## Related Areas

- [`../ROADMAP.md`](../ROADMAP.md) — milestone sequencing and acceptance criteria.
- [`../FEATURES.md`](../FEATURES.md) — implemented/planned capability inventory.
- [`../../AGENTS.md`](../../AGENTS.md) — project-specific agent ground rules.
