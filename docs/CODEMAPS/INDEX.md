# Iris Current Codemap

**Last Updated:** 2026-06-15
**Entry Points:** `src/main.rs`

This codemap describes implemented code only. Planned capabilities live in [`../ROADMAP.md`](../ROADMAP.md) and [`../FEATURES.md`](../FEATURES.md).

## Architecture

╭──────────╮     ╭──────────────╮     ╭────────────────────────────╮
│ Iris CLI │────▶│ Nexus Agent  │────▶│ OpenAI Codex Responses     │
│ main.rs  │     │ nexus.rs     │     │ provider                   │
╰────┬─────╯     ╰──────┬───────╯     ╰─────────────┬──────────────╯
     │                  │                           │
     │                  ▼                           ▼
     │           ╭──────────────╮      ╭────────────────────────────╮
     │           │ Built-in     │      │ OpenAI Codex OAuth auth    │
     │           │ tools.rs     │      │ token store / refresh      │
     │           ╰──────────────╯      ╰────────────────────────────╯
     │
     ▼
╭────────────────────────────╮
│ OpenAI Codex login flows   │
│ browser / device code      │
╰────────────────────────────╯

## Key Modules

| Module | Purpose | Public/internal API | Dependencies |
|---|---|---|---|
| `src/main.rs` | CLI entrypoint. Starts the agent loop or runs OpenAI Codex login commands. | `main()` | `nexus::Agent`, `OpenAiCodexResponsesProvider`, `auth::openai_codex` |
| `src/nexus.rs` | Runtime core for the current text-only REPL. Maintains conversation state, calls a provider, executes tool calls, and records tool results. | `ChatProvider`, `Agent`, `AssistantTurn`, `ToolCall`, `Message`, `Role` | `std::io`, `anyhow`, `serde_json`, `crate::tools` |
| `src/tools.rs` | Workspace-scoped implementations and JSON declarations for built-in tools. | `dispatch()`, `tool_definitions()` | filesystem/path/process APIs, `anyhow`, `serde`, `serde_json`, `xxhash-rust` |
| `src/auth/mod.rs` | Auth module declaration. | `device_code`, `openai_codex`, `storage` modules | auth submodules |
| `src/auth/storage.rs` | Provider-keyed auth-file storage for OAuth credentials. Reads missing files as empty, validates credential shape, and writes atomically with restricted Unix permissions. | `AuthStore`, `OAuthCredentials` | filesystem/env APIs, `anyhow`, `serde`, `serde_json` |
| `src/auth/device_code.rs` | Generic polling helper for OAuth device-code flows. | `DeviceCodePoll`, `poll_device_code()` | `std::thread`, `std::time`, `anyhow` |
| `src/auth/openai_codex.rs` | OpenAI Codex OAuth integration. Supports browser callback login, device-code login, token exchange/refresh, and account ID extraction from JWT payloads. | `OpenAiCodexTokenStore`, `AccessToken`, `login_browser()`, `login_device_code()` | `AuthStore`, `poll_device_code`, `base64`, `rand`, `reqwest`, `sha2`, `serde`, `serde_json`, TCP/filesystem/time APIs |
| `src/providers/mod.rs` | Provider module declaration. | `openai_codex_responses` module | `src/providers/openai_codex_responses.rs` |
| `src/providers/openai_codex_responses.rs` | Implements `ChatProvider` for the ChatGPT Codex Responses endpoint. Builds request JSON, headers, URL, advertises tools, and parses streamed assistant text/tool calls. | `OpenAiCodexResponsesProvider` | `OpenAiCodexTokenStore`, `ChatProvider`, Nexus message/turn types, `crate::tools`, `reqwest`, `serde_json` |

## Data Flow

1. `main()` creates `OpenAiCodexResponsesProvider::from_env()` and starts `Agent::run()`.
2. `Agent::run()` starts a REPL and delegates to `run_with()`.
3. User prompts are appended as `Message { role: User, content }`.
4. `ChatProvider::respond()` receives the full in-memory message list.
5. The OpenAI Codex provider reads or refreshes OAuth credentials.
6. The provider converts Nexus messages into Codex Responses request JSON and includes built-in tool definitions from `tools.rs`.
7. The provider sends a blocking HTTP request and parses streamed response events into assistant text and tool calls.
8. Nexus prints `assistant> ...` for text and appends assistant text to conversation state.
9. For each tool call, Nexus prints `tool> name(args)`, records the assistant tool call, dispatches to `tools.rs`, and records a JSON `{ ok, content/error }` tool result.
10. Nexus repeats provider calls until the assistant returns no tool calls or the bounded tool-iteration limit is exceeded.
11. Provider errors are printed to stderr and the REPL continues.

## Configuration and Inputs

| Input | Default | Used by |
|---|---|---|
| `IRIS_AUTH_PATH` | `~/.iris/auth.json` | `OpenAiCodexTokenStore::from_env()` |
| `IRIS_MODEL` | `gpt-5.5` | `OpenAiCodexResponsesConfig::from_env()` |
| `IRIS_CODEX_BASE_URL` | `https://chatgpt.com/backend-api` | `OpenAiCodexResponsesConfig::from_env()` |
| `HOME` | required when `IRIS_AUTH_PATH` is unset | auth path resolution |

## CLI Commands

| Command | Purpose |
|---|---|
| `iris-agent` | Start the interactive agent in the current working directory. |
| `iris-agent login openai-codex` | Run browser OAuth login using a local callback server. |
| `iris-agent login openai-codex --browser` | Explicit browser OAuth login. |
| `iris-agent login openai-codex --device-code` | Run device-code OAuth login. |
| `iris-agent help` / `--help` / `-h` | Print command help. |

## Built-in Tools

| Tool | Purpose | Safety boundary |
|---|---|---|
| `read` | Read text files with truncation, offset/limit, and optional hashline tags. | Existing path must resolve inside the workspace. |
| `write` | Create or overwrite files, creating parent directories as needed. | Target path and existing ancestors must remain inside the workspace. |
| `edit` | Replace a unique text match, with whitespace-normalized fallback matching. | Existing path must resolve inside the workspace. |
| `bash` | Run a bounded shell command in the workspace with captured stdout/stderr and timeout handling. | Command cwd is the workspace. Approval is not implemented yet. |
| `grep` | Search workspace content through `rg` when available. | Search path resolves inside the workspace. |
| `find` | Find workspace files through `fd`/`fdfind` when available. | Search path resolves inside the workspace. |
| `ls` | List directory entries with a scan limit. | Directory path resolves inside the workspace. |
| `hashline_edit` | Apply content-hash anchored line edits using `read` hashline tags. | Existing path must resolve inside the workspace. |

## External Dependencies

- `anyhow` — error propagation and context.
- `base64` — base64url JWT payload decoding.
- `reqwest` — blocking HTTP client with JSON and rustls TLS.
- `rand` — OAuth PKCE/state token generation.
- `serde` — auth-file serialization/deserialization.
- `serde_json` — JSON request/response construction and parsing.
- `sha2` — OAuth PKCE challenge hashing.
- `xxhash-rust` — hashline tag generation and validation.

## Tests

Current unit tests cover:

- REPL conversation persistence and provider-error recovery in `src/nexus.rs`.
- Tool-call loop execution, too-many-tool-calls errors, tool error encoding, and workspace path/symlink rejection in `src/nexus.rs`.
- Built-in tool behavior in `src/tools.rs`, including read/write/edit, hashline edits, `ls`, optional `grep`/`find` integration, bash output/timeout/nonzero handling, and dispatch/tool-definition coverage.
- Auth storage parsing and atomic restricted writes in `src/auth/storage.rs`.
- Device-code polling behavior in `src/auth/device_code.rs`.
- JWT account extraction, browser OAuth URL/callback parsing, device-code interval parsing, and device-auth error parsing in `src/auth/openai_codex.rs`.
- Codex URL resolution, request JSON construction, streamed text parsing, tool-call parsing, and missing-output errors in `src/providers/openai_codex_responses.rs`.

## Known Gaps

The Agent Kernel MVP is not complete. Missing runtime areas include approval prompts and denied-call handling, incremental terminal streaming, session persistence, and the later roadmap systems listed in `ROADMAP.md`.

## Related Areas

- [`../ROADMAP.md`](../ROADMAP.md) — milestone sequencing and acceptance criteria.
- [`../FEATURES.md`](../FEATURES.md) — implemented/planned capability inventory.
- [`../../AGENTS.md`](../../AGENTS.md) — project-specific agent ground rules.
