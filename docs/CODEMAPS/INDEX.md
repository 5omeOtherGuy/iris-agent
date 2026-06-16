# Iris Current Codemap

**Last Updated:** 2026-06-17
**Entry Points:** `src/main.rs`

This codemap describes implemented code only. Planned capabilities live in [`../ROADMAP.md`](../ROADMAP.md) and [`../FEATURES.md`](../FEATURES.md).

## Architecture

╭──────────────╮   ╭──────────────╮   ╭──────────────╮   ╭────────────────────────────╮
│ Iris CLI     │──▶│ cli.rs       │──▶│ Nexus Agent  │──▶│ OpenAI Codex Responses     │
│ main.rs      │   │ run_session  │   │ nexus.rs     │   │ provider                   │
╰──────┬───────╯   ╰──────┬───────╯   ╰──────┬───────╯   ╰─────────────┬──────────────╯
       │                  │ Ui trait         │ UiEvent /                │
       │                  ▼ (events)          │ TurnSink                ▼
       │           ╭──────────────╮   ╭───────┴────────╮     ╭────────────────────────────╮
       │           │ ui/ (TextUi) │   ▼                ▼     │ OpenAI Codex OAuth auth    │
       │           │ tool_display │  ╭──────────────╮ ╭─────╮│ token store / refresh      │
       │           ╰──────────────╯  │ Built-in     │ │ diff│╰────────────────────────────╯
       │                             │ tools/       │ │ prev│
       ▼                             ╰──────────────╯ ╰─────╯
╭────────────────────────────╮
│ OpenAI Codex login flows   │
│ browser / device code      │
╰────────────────────────────╯

Nexus is provider- and UI-neutral: it drives turns and approval policy, streams
text through a `TurnSink`, and renders nothing itself. All terminal I/O lives
behind the `Ui` trait; the only implementation today is the text front-end. The
next planned runtime change is documented in [`../ROADMAP.md`](../ROADMAP.md):
replace the current blocking provider/tool seams with an async stream/cancel
runtime while preserving this ownership split.

## Key Modules

| Module | Purpose | Public/internal API | Dependencies |
|---|---|---|---|
| `src/main.rs` | CLI entrypoint. Initializes telemetry, parses args, constructs the bare agent + provider + tools, wraps them in a Tier-2 `wayland::Harness` (with optional session log), runs the session or OpenAI Codex login commands, and maps typed errors to process exit codes. | `main()`, `dispatch()` | `cli`, `nexus::Agent`, `wayland::Harness`, `ui::text::TextUi`, `OpenAiCodexResponsesProvider`, `auth::openai_codex`, `telemetry`, `errors` |
| `src/cli.rs` | Iris CLI session loop. Reads prompts through the `Ui` seam, skips blanks, exits on `/exit`/`/quit`, and submits each turn on the `wayland::Harness` via a per-turn `UiBridge` that backs both Nexus seams. | `run_session()` | `wayland::Harness`, `nexus::ChatProvider`, `ui::{Ui, UiBridge, UiEvent, is_exit_command}` |
| `src/nexus.rs` | Runtime core (Tier 1). A provider-, UI-, persistence-, and workspace-neutral in-memory engine: owns conversation state + the injected `Tools` + approval policy, calls the provider with a streaming sink, enforces approval before gated tools, executes tools against an injected `&mut ToolEnv`, and emits `AgentEvent`s to an `AgentObserver`; gates tools via `ApprovalGate`. Holds no filesystem or session store. Bounds the tool loop and ends gracefully at the cap. | `ChatProvider`, `TurnSink`, `Agent`, `Agent::submit_turn()`, `Agent::messages()`, `Tool`, `Tools`, `ToolEnv`, `ToolOutput`, `AgentEvent`, `AgentObserver`, `ApprovalGate`, `ApprovalDecision`, `AssistantTurn`, `ToolCall`, `Message`, `Role` | `anyhow`, `serde_json`, `tracing`, `crate::{tools, signals}` |
| `src/wayland.rs` | Tier-2 harness. Wraps the bare `nexus::Agent`, owns the execution surface (workspace + `tools::ToolState`) and the optional `session::SessionLog`, injects a `ToolEnv` into each turn, and persists new transcript messages post-turn (best-effort, diffing `agent.messages()`). Mirrors pi's `AgentHarness`. | `Harness`, `Harness::new()`, `Harness::submit_turn()` | `anyhow`, `tracing`, `crate::{nexus, session, tools}` |
| `src/ui/mod.rs` | Terminal front-end seam (Tier 3). Defines the `Ui` trait, the `UiEvent` render protocol, turn-error classification, exit-command parsing, and `UiBridge` (adapts a `Ui` onto the Nexus `AgentObserver`/`ApprovalGate` seams via `RefCell`). | `Ui`, `UiEvent`, `UiBridge`, `TurnErrorKind`, `is_exit_command()` | `anyhow`, `crate::{nexus, errors}` |
| `src/ui/text.rs` | Text terminal front-end. Owns stdin/stdout/stderr, prints the `iris>` prompt, renders streamed assistant deltas and tool lifecycle lines via `tool_display`, prompts for approval, and routes auth/provider errors to stderr. | `TextUi`, `TextUi::stdio()` | `std::io`, `crate::{approval, nexus, ui, tool_display}` |
| `src/tool_display.rs` | Presentation-only formatter for tool-call lines (proposed/approval/denied/result/error). Returns owned strings, performs no I/O, and never changes what is sent to the model. | `summarize()`, `proposed_line()`, `approval_prompt()`, `denied_line()`, `result_line()`, `error_line()` | `serde_json`, `crate::nexus::ToolCall` |
| `src/approval.rs` | Terminal decision parser (Tier 3). Translates a typed line into the Tier-1 `crate::nexus::ApprovalDecision`: `y`/`yes` allow, `a`/`always` allow-session, anything else denies. | `parse_decision()` | `crate::nexus` |
| `src/errors.rs` | Provider-neutral typed errors carried across runtime boundaries for user-facing handling and exit codes. | `AuthError`, `UsageError`, `exit_code()` | `thiserror` |
| `src/telemetry.rs` | Operator observability: `RUST_LOG`-driven tracing to stderr, secret-safe fingerprints, and sanitization of external response bodies before they reach logs/errors. | `init()`, `redact_secret()`, `sanitize_external_body()` | `tracing-subscriber`, `sha2`, `serde_json` |
| `src/config.rs` | Iris settings file loader. Reads `~/.iris/settings.json` (global) and `<cwd>/.iris/settings.json` (project), merges field-by-field with project winning, ignores unknown keys, and errors loudly on malformed JSON. Provides provider/model/base-url defaults; env still overrides via the provider. | `Settings`, `Settings::load()` | `serde`, `serde_json`, `anyhow`, env/filesystem APIs |
| `src/session.rs` | Best-effort JSONL transcript persistence. Writes a `session` header then appends one `message` line per conversation entry, flushing each write so a crash leaves a valid prefix. Linear transcript only (no tree/branching/compaction). | `SessionLog`, `SessionLog::create()`, `SessionLog::append()`, `SessionLog::path()` | `serde_json`, `anyhow`, `rand`, filesystem/time APIs, `crate::nexus::Message` |
| `src/signals.rs` | Graceful SIGINT handling for the REPL. First Ctrl-C sets an interrupt flag the tool loop checks between round-trips (ends the turn cleanly); a second reaps tracked process groups via `process_group`, restores the default handler, and re-raises to force-quit. | `install()`, `interrupted()`, `reset()` | `libc`, `crate::process_group`, atomics |
| `src/process_group.rs` | Single owner of process-group spawn/kill/reap policy for `bash` shells. Puts commands in their own group, kills+reaps groups, and keeps a lock-free registry so the force-quit SIGINT handler can SIGKILL every live group with only async-signal-safe ops. | `in_own_group()`, `kill()`, `kill_and_reap()`, `register()`, `kill_all_from_signal()`, `GroupGuard` | `libc`, atomics, `std::process` |
| `src/tools/mod.rs` | Built-in tool dispatch, the `ToolOutput { content, metadata }` result contract, JSON tool declarations, mutating-tool approval classifier, diff-preview generation, and shared external-binary lookup. | `dispatch()`, `ToolOutput`, `requires_approval()`, `diff_preview()`, `tool_definitions()` | tool submodules, `anyhow`, `serde_json`, `std::process` |
| `src/tools/path.rs` | Workspace path resolution and display helpers. Canonicalizes existing paths, normalizes create targets, and rejects workspace escapes. | `workspace_root()`, `resolve_existing()`, `resolve_for_write()`, `relative_display()` | `std::path`, `anyhow` |
| `src/tools/text.rs` | Shared text, truncation, size-limit, line-ending, and atomic-write helpers. | `atomic_write()`, `truncate_head()`, `truncate_tail()`, line-ending helpers | filesystem APIs, `rand`, `anyhow` |
| `src/tools/read.rs` | Text-file read tool with offset/limit, line numbers, binary/NUL and invalid UTF-8 rejection. | `execute()` | `path`, `text`, filesystem APIs, `serde` |
| `src/tools/write.rs` | Create/overwrite tool. Creates parents, writes through symlinks inside the workspace, and uses atomic replacement. | `execute()` | `path`, `text`, filesystem APIs, `serde` |
| `src/tools/edit.rs` | Claude-compatible exact-string replacement (`file_path`/`old_string`/`new_string`/`replace_all`) with fuzzy fallback matching, BOM/EOL preservation, no-op rejection, stale-file preflight, and atomic replacement. | `execute()` | `path`, `text`, `observe`, filesystem APIs, `serde` |
| `src/tools/observe.rs` | Session-scoped file observation store for stale-file detection: records `{mtime, content_hash}` per canonical path on read/write/edit and rejects mutating an existing file that was never read or changed since last read. | `ObservedFiles::observe()`, `ObservedFiles::ensure_fresh()` | `sha2`, filesystem APIs |
| `src/tools/bash/mod.rs` | Shell command tool dispatch and per-agent `BashState`. One-shot runs with cwd confinement, timeout, process-group kill, bounded output drain/truncation, and nonzero-exit reporting; routes `session`/`action` and background-job actions to the submodules. | `execute()`, `BashState`, `parameters()` | `bash::{session, jobs, sandbox}`, `process_group`, process/filesystem APIs, `serde` |
| `src/tools/bash/sandbox.rs` | Kernel sandbox (Linux Landlock LSM): confines each shell to write only the workspace (plus `/dev/null`) and denies TCP networking, enforced in the child via `pre_exec`. Explicit non-silent fallback with a surfaced notice when Landlock is unavailable. | `confine()`, `SandboxStatus` | `landlock`, `libc`, `std::os::unix` |
| `src/tools/bash/session.rs` | Persistent shell sessions: a long-lived `bash` co-process where `cd`/`export`/vars survive across calls, delimited by a high-entropy sentinel-marker protocol with exit-code parsing. | `Sessions`, `run`/`reset`/`close` | `process_group`, `sandbox`, process/thread APIs |
| `src/tools/bash/jobs.rs` | Background jobs: start a detached confined command, poll new output from a bounded byte ring addressed by absolute cursor, finalize (bounded wait) for the exit code, list, and cancel. | `Jobs`, `start`/`poll`/`finalize`/`list`/`cancel` | `process_group`, `sandbox`, threads/condvar |
| `src/tools/grep.rs` | Library-backed (grep/ignore) content search, grouped by file with context. | `execute()` | `path`, `text`, `grep`, `ignore`, `serde` |
| `src/tools/find.rs` | Native (ignore + globset) file glob search sorted newest-first. | `execute()` | `path`, `text`, `ignore`, `globset`, `serde` |
| `src/tools/ls.rs` | Directory listing tool: directories first, dotfiles, directory suffixes, optional recursive tree, optional `long` mode (type marker + human-readable size), entry-count metadata, and output caps. | `execute()` | `path`, `text`, filesystem APIs, `serde` |
| `src/auth/mod.rs` | Auth module declaration. | `device_code`, `openai_codex`, `storage` modules | auth submodules |
| `src/auth/storage.rs` | Provider-keyed auth-file storage for OAuth credentials. Reads missing files as empty, validates credential shape, and writes atomically with restricted Unix permissions. | `AuthStore`, `OAuthCredentials` | filesystem/env APIs, `anyhow`, `serde`, `serde_json` |
| `src/auth/device_code.rs` | Generic polling helper for OAuth device-code flows. | `DeviceCodePoll`, `poll_device_code()` | `std::thread`, `std::time`, `anyhow` |
| `src/auth/openai_codex.rs` | OpenAI Codex OAuth integration. Supports browser callback login, device-code login, token exchange/refresh, and account ID extraction from JWT payloads. | `OpenAiCodexTokenStore`, `AccessToken`, `login_browser()`, `login_device_code()` | `AuthStore`, `poll_device_code`, `base64`, `rand`, `reqwest`, `sha2`, `serde`, `serde_json`, TCP/filesystem/time APIs |
| `src/providers/mod.rs` | Provider module declaration. | `openai_codex_responses` module | `src/providers/openai_codex_responses.rs` |
| `src/providers/openai_codex_responses.rs` | Implements `ChatProvider` for the ChatGPT Codex Responses endpoint. Builds request JSON/headers/URL, advertises tools, retries with backoff, and parses streamed assistant text (via `TurnSink`) and tool calls. | `OpenAiCodexResponsesProvider` | `OpenAiCodexTokenStore`, `ChatProvider`, `TurnSink`, Nexus message/turn types, `crate::{tools, errors, telemetry}`, `reqwest`, `serde_json`, `tracing` |

## Data Flow

1. `main()` calls `telemetry::init()`, installs the SIGINT handler via `signals::install()`, and runs `dispatch()`.
2. For the default command, `run_agent()` loads `config::Settings` for the cwd (rejecting an unsupported `default_provider`), builds `OpenAiCodexResponsesProvider::new()` from the settings model/base-url, creates an `Agent` rooted at the current dir, attaches a best-effort `session::SessionLog` (warns and continues in-memory if it cannot be opened), and a stdio `TextUi`, then calls `cli::run_session()`.
3. `run_session()` emits `SessionStarted`, then loops: read a prompt through `Ui::next_prompt()`, skip blanks, break on `/exit`/`/quit`, and call `Agent::submit_turn(prompt, ui)`.
4. `submit_turn()` appends `Message::user(prompt)` and runs `complete_turn()`.
5. `complete_turn()` calls `ChatProvider::respond(messages, sink)` with a `UiTurnSink`; the provider streams assistant text as `AssistantTextDelta` events through the sink.
6. The OpenAI Codex provider reads or refreshes OAuth credentials, converts Nexus messages to Codex Responses request JSON (with tool definitions from `tools::tool_definitions()`), sends a blocking request with retry/backoff, and parses streamed events into deltas, final text, and tool calls.
7. Nexus commits final assistant text as `AssistantText`/`AssistantTextEnd` and appends it to conversation state.
8. With no tool calls, Nexus emits `TurnComplete` and returns.
9. For each tool call, Nexus records the assistant tool call. Gated tools (`tools::requires_approval()`) emit a `DiffPreview` when `tools::diff_preview()` returns one, then `Ui::request_approval()` collects a decision; denial emits `ToolDenied` and records `{ ok: false, denied: true }`. Ungated tools emit `ToolProposed`.
10. Allowed or ungated calls dispatch through `tools::dispatch()`; Nexus emits `ToolResult`/`ToolError` for display and records the full JSON `{ ok, content/error }` result for the model.
11. The loop repeats until the assistant returns no tool calls or the bounded `MAX_TOOL_ROUNDTRIPS` cap is hit, at which point Nexus emits a `Notice` and `TurnComplete` (graceful, not an error). Between round-trips Nexus checks `signals::interrupted()`: a pending Ctrl-C ends the turn cleanly (pending tools denied) and returns to the prompt.
12. When a session log is attached, each newly committed message is appended to the JSONL transcript as the turn progresses.
13. Turn errors from `submit_turn()` are classified by `UiEvent::from_turn_error()` into auth vs provider and rendered to stderr; the session continues.

## Configuration and Inputs

| Input | Default | Used by |
|---|---|---|
| `~/.iris/settings.json`, `<cwd>/.iris/settings.json` | absent (built-in defaults) | `config::Settings::load()` (project overrides global field-by-field) |
| `IRIS_AUTH_PATH` | `~/.iris/auth.json` | `OpenAiCodexTokenStore::from_env()` |
| `IRIS_CONFIG_PATH` | `~/.iris/settings.json` | global settings path override (`config::Settings`) |
| `IRIS_SESSION_DIR` | `~/.iris/sessions` | transcript root (`session::SessionLog`) |
| `IRIS_MODEL` | `gpt-5.5` | `OpenAiCodexResponsesConfig::resolve()` (env > settings > default) |
| `IRIS_CODEX_BASE_URL` | `https://chatgpt.com/backend-api` | `OpenAiCodexResponsesConfig::resolve()` (env > settings > default) |
| `RUST_LOG` | `warn` | `telemetry::init()` tracing filter |
| `HOME` | required when the matching path override is unset | auth/settings/session path resolution |

## CLI Commands

| Command | Purpose |
|---|---|
| `iris-agent` | Start the interactive agent session in the current working directory. |
| `iris-agent login openai-codex` | Run browser OAuth login using a local callback server. |
| `iris-agent login openai-codex --browser` | Explicit browser OAuth login. |
| `iris-agent login openai-codex --device-code` | Run device-code OAuth login. |
| `iris-agent help` / `--help` / `-h` | Print command help. |

Unknown commands print help and exit with code `2` (`UsageError`); auth failures exit `3` (`AuthError`); other errors exit `1`.

## Built-in Tools

| Tool | Purpose | Safety boundary |
|---|---|---|
| `read` | Read text files with truncation, offset/limit, and invalid UTF-8/binary rejection. | Existing path must resolve inside the workspace. |
| `write` | Create or overwrite files, creating parent directories as needed and writing atomically. | Target path and existing ancestors must remain inside the workspace; approval-gated with diff preview. |
| `edit` | Replace a unique exact-string match (Claude-compatible schema; `replace_all` for every occurrence), with whitespace-normalized fallback matching and atomic writes. | Existing path must resolve inside the workspace; approval-gated with diff preview. |
| `bash` | Run a shell command in the workspace with captured stdout/stderr, timeout handling, and process-group cleanup. Supports one-shot runs, persistent sessions (`session`/`action`, state carries across calls), and background jobs (start/poll/finalize/list/cancel). | Command cwd is the workspace; kernel-confined via Landlock (workspace-write, TCP-deny) where available; approval-gated. |
| `grep` | Search workspace content in-process via the ripgrep library crates. | Search path resolves inside the workspace. |
| `find` | Find workspace files in-process via `ignore` + `globset`. | Search path resolves inside the workspace. |
| `ls` | List directory entries (directories first, optional recursive tree, optional `long` type+size mode) with a scan limit. | Directory path resolves inside the workspace. |

## External Dependencies

- `anyhow` — error propagation and context.
- `base64` — base64url JWT payload decoding.
- `landlock` — Linux Landlock LSM ruleset construction for the `bash` kernel sandbox.
- `libc` — Unix process-group spawn/termination/reaping and async-signal-safe SIGINT handling.
- `rand` — OAuth PKCE/state token generation and unique atomic-write temp names.
- `reqwest` — blocking HTTP client with JSON and rustls TLS.
- `serde` — auth-file and request/response serialization.
- `serde_json` — JSON request/response construction and parsing.
- `sha2` — OAuth PKCE challenge hashing, telemetry secret fingerprints, and file-observation content hashing.
- `similar` — diff generation for mutating-tool previews.
- `thiserror` — typed boundary error definitions (`AuthError`, `UsageError`).
- `tracing` / `tracing-subscriber` — structured logging to stderr, gated by `RUST_LOG`.

## Tests

Current unit tests cover:

- Session loop, conversation persistence, streamed-delta rendering, and auth/provider error recovery in `src/nexus.rs` and `src/cli.rs`.
- Tool-call loop execution, graceful round-trip limiting, diff-preview-before-approval ordering, tool error encoding, approval allow/deny handling, and workspace path/symlink rejection in `src/nexus.rs`.
- Terminal decision parsing in `src/approval.rs`.
- Typed-error exit-code classification (including through `context` wrapping) in `src/errors.rs`.
- Secret redaction and external-body sanitization in `src/telemetry.rs`.
- Tool-call display formatting in `src/tool_display.rs`.
- Settings file loading/merge precedence, unknown-key tolerance, and malformed-file errors in `src/config.rs`.
- JSONL transcript header/append/tool-call entries and cwd slugging in `src/session.rs`.
- SIGINT first-press/repeat flag behavior in `src/signals.rs`.
- Process-group registration/guard, targeted kill, and backgrounded-grandchild reaping in `src/process_group.rs`.
- Built-in tool behavior under `src/tools/`, including read/write/edit, atomic writes, `ls`, optional `grep`/`find` integration, bash output/timeout/process-group handling, persistent sessions, background jobs, Landlock sandbox decision/fallback, diff previews, and dispatch/tool-definition coverage.
- Larger Nexus and Codex-provider suites split into `src/nexus_tests.rs` and `src/providers/openai_codex_responses_tests.rs`.
- Auth storage parsing and atomic restricted writes in `src/auth/storage.rs`.
- Device-code polling behavior in `src/auth/device_code.rs`.
- JWT account extraction, browser OAuth URL/callback parsing, device-code interval parsing, and device-auth error parsing in `src/auth/openai_codex.rs`.
- Codex URL resolution, request JSON construction, streamed text/delta parsing, tool-call parsing, and missing-output errors in `src/providers/openai_codex_responses.rs`.

## Known Gaps

Milestone 1 is complete, but the runtime is not finished. The important next gap
is runtime hardness: `ChatProvider` is not yet an async streaming contract,
active turns do not yet own a real cancellation token, provider reads and tool
futures are not raced against cancellation, tools do not yet receive child
cancellation tokens, and concurrency-safe tools do not yet run in parallel under
Nexus scheduling. Also still missing: persistent approval policies, session
`/resume` and transcript-tree branching, and the later roadmap systems listed in
[`../ROADMAP.md`](../ROADMAP.md).

## Related Areas

- [`../ROADMAP.md`](../ROADMAP.md) — milestone sequencing and acceptance criteria.
- [`../FEATURES.md`](../FEATURES.md) — implemented/planned capability inventory.
- [`../../AGENTS.md`](../../AGENTS.md) — project-specific agent ground rules.
