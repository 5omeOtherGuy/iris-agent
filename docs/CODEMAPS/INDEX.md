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
     │          ╭───────┴────────╮                  ▼
     │          ▼                ▼        ╭────────────────────────────╮
     │   ╭──────────────╮ ╭──────────────╮│ OpenAI Codex OAuth auth    │
     │   │ Built-in     │ │ Approver     ││ token store / refresh      │
     │   │ tools/       │ │ approval.rs  │╰────────────────────────────╯
     │   ╰──────────────╯ ╰──────────────╯
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
| `src/nexus.rs` | Runtime core for the current text-only REPL. Maintains conversation state, calls a provider, enforces approval before gated tools, executes tool calls, and records tool results. | `ChatProvider`, `Agent`, `AssistantTurn`, `ToolCall`, `Message`, `Role` | `std::io`, `anyhow`, `serde_json`, `crate::approval`, `crate::tools` |
| `src/approval.rs` | Approval seam between Nexus and terminal UI. Parses `y`/`yes` as allow; EOF/anything else denies. | `ApprovalDecision`, `Approver`, `TerminalApprover` | `std::io`, `anyhow`, `crate::nexus::ToolCall` |
| `src/tools/mod.rs` | Built-in tool dispatch, tool JSON declarations, mutating-tool approval classifier, and shared external-binary lookup. | `dispatch()`, `requires_approval()`, `tool_definitions()` | tool submodules, `anyhow`, `serde_json`, `std::process` |
| `src/tools/path.rs` | Workspace path resolution and display helpers. Canonicalizes existing paths, normalizes create targets, and rejects workspace escapes. | `workspace_root()`, `resolve_existing()`, `resolve_for_write()`, `relative_display()` | `std::path`, `anyhow` |
| `src/tools/text.rs` | Shared text, truncation, size-limit, line-ending, and atomic-write helpers. | `atomic_write()`, `truncate_head()`, `truncate_tail()`, line-ending helpers | filesystem APIs, `rand`, `anyhow` |
| `src/tools/read.rs` | Text-file read tool with offset/limit, line numbers, hashline rendering, binary/NUL and invalid UTF-8 rejection. | `execute()` | `path`, `text`, `hashline`, filesystem APIs, `serde` |
| `src/tools/write.rs` | Create/overwrite tool. Creates parents, writes through symlinks inside the workspace, and uses atomic replacement. | `execute()` | `path`, `text`, filesystem APIs, `serde` |
| `src/tools/edit.rs` | Targeted text replacement with exact/fuzzy unique matching, BOM/EOL preservation, no-op rejection, and atomic replacement. | `execute()` | `path`, `text`, filesystem APIs, `serde` |
| `src/tools/hashline.rs` | Hashline tag algorithm plus hash-anchored edit tool. Validates anchors, applies edits bottom-up, preserves BOM/EOL, and writes atomically. | `execute()`, `format_hashline_tag()` | `path`, `text`, `xxhash-rust`, filesystem APIs, `serde` |
| `src/tools/bash.rs` | Shell command tool with cwd confinement, timeout, process-group kill, output drain, truncation, and nonzero-exit reporting. | `execute()` | process/filesystem APIs, `libc` on Unix, `serde` |
| `src/tools/grep.rs` | Ripgrep-backed content search with workspace-relative output and optional hashline tags for match/context lines. | `execute()` | `path`, `text`, `hashline`, `std::process`, `serde` |
| `src/tools/find.rs` | fd/fdfind-backed file glob search sorted newest-first. | `execute()` | `path`, `text`, filesystem/process APIs, `serde` |
| `src/tools/ls.rs` | Directory listing tool with sorted entries, dotfiles, directory suffixes, and output caps. | `execute()` | `path`, `text`, filesystem APIs, `serde` |
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
6. The provider converts Nexus messages into Codex Responses request JSON and includes built-in tool definitions from `tools/mod.rs`.
7. The provider sends a blocking HTTP request and parses streamed response events into assistant text and tool calls.
8. Nexus prints `assistant> ...` for text and appends assistant text to conversation state.
9. For each tool call, Nexus prints `tool> name(args)` and records the assistant tool call.
10. If `tools::requires_approval()` gates the tool, Nexus asks the live `Approver`; denial skips execution, prints `denied> ...`, and records `{ ok: false, denied: true, error }` as the tool result.
11. Allowed or ungated calls dispatch through `tools/mod.rs`; Nexus prints a capped result/error for the user and records the full JSON `{ ok, content/error }` tool result for the model.
12. Nexus repeats provider calls until the assistant returns no tool calls or the bounded tool-iteration limit is exceeded.
13. Provider errors are printed to stderr and the REPL continues.

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
| `read` | Read text files with truncation, offset/limit, optional hashline tags, and invalid UTF-8/binary rejection. | Existing path must resolve inside the workspace. |
| `write` | Create or overwrite files, creating parent directories as needed and writing atomically. | Target path and existing ancestors must remain inside the workspace; approval-gated. |
| `edit` | Replace a unique text match, with whitespace-normalized fallback matching and atomic writes. | Existing path must resolve inside the workspace; approval-gated. |
| `bash` | Run a bounded shell command in the workspace with captured stdout/stderr, timeout handling, and process-group cleanup. | Command cwd is the workspace; approval-gated. |
| `grep` | Search workspace content through `rg` when available, with optional hashline tags. | Search path resolves inside the workspace. |
| `find` | Find workspace files through `fd`/`fdfind` when available. | Search path resolves inside the workspace. |
| `ls` | List directory entries with a scan limit. | Directory path resolves inside the workspace. |
| `hashline_edit` | Apply content-hash anchored line edits using `read`/`grep` hashline tags and atomic writes. | Existing path must resolve inside the workspace; approval-gated. |

## External Dependencies

- `anyhow` — error propagation and context.
- `base64` — base64url JWT payload decoding.
- `libc` — Unix process-group termination for bash timeout cleanup.
- `reqwest` — blocking HTTP client with JSON and rustls TLS.
- `rand` — OAuth PKCE/state token generation and unique atomic-write temp names.
- `serde` — auth-file serialization/deserialization.
- `serde_json` — JSON request/response construction and parsing.
- `sha2` — OAuth PKCE challenge hashing.
- `xxhash-rust` — hashline tag generation and validation.

## Tests

Current unit tests cover:

- REPL conversation persistence and provider-error recovery in `src/nexus.rs`.
- Tool-call loop execution, graceful round-trip limiting, tool error encoding, approval allow/deny handling, and workspace path/symlink rejection in `src/nexus.rs`.
- Terminal approval parsing and EOF-deny behavior in `src/approval.rs`.
- Built-in tool behavior under `src/tools/`, including read/write/edit, hashline edits, atomic writes, `ls`, optional `grep`/`find` integration, bash output/timeout/process-group handling, and dispatch/tool-definition coverage.
- Auth storage parsing and atomic restricted writes in `src/auth/storage.rs`.
- Device-code polling behavior in `src/auth/device_code.rs`.
- JWT account extraction, browser OAuth URL/callback parsing, device-code interval parsing, and device-auth error parsing in `src/auth/openai_codex.rs`.
- Codex URL resolution, request JSON construction, streamed text parsing, tool-call parsing, and missing-output errors in `src/providers/openai_codex_responses.rs`.

## Known Gaps

The Agent Kernel MVP is not complete. Missing runtime areas include incremental terminal streaming, session persistence, diff previews, persistent approval policies, shared file observation/stale-mutation preflight, structured tool-result metadata, and the later roadmap systems listed in `ROADMAP.md`.

## Related Areas

- [`../ROADMAP.md`](../ROADMAP.md) — milestone sequencing and acceptance criteria.
- [`../FEATURES.md`](../FEATURES.md) — implemented/planned capability inventory.
- [`../../AGENTS.md`](../../AGENTS.md) — project-specific agent ground rules.
