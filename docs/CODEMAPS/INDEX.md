# Iris Current Codemap

**Last Updated:** 2026-06-22
**Entry Points:** `src/main.rs`

This codemap describes implemented code only. Planned capabilities live in [`../ROADMAP.md`](../ROADMAP.md) and [`../FEATURES.md`](../FEATURES.md).

## Architecture

╭──────────────╮   ╭──────────────╮   ╭──────────────╮   ╭────────────────────────────╮
│ Iris CLI     │──▶│ cli.rs       │──▶│ Nexus Agent  │──▶│ Mimir provider adapter     │
│ main.rs      │   │ run_session  │   │ nexus.rs     │   │ chosen at startup          │
╰──────┬───────╯   ╰──────┬───────╯   ╰──────┬───────╯   ╰─────────────┬──────────────╯
       │                  │ Ui trait         │ UiEvent /                │
       │                  ▼ (events)          │ ProviderEvent           ▼
       │           ╭──────────────╮   ╭───────┴────────╮     ╭────────────────────────────╮
       │           │ ui/ (TUI/text)│   ▼                ▼     │ auth store / refresh       │
       │           │ tool_display │  ╭──────────────╮ ╭─────╮│ OpenAI / Anthropic /       │
       │           ╰──────────────╯  │ Built-in     │ │ diff││ Antigravity                │
       │                             │ tools/       │ │ prev│╰────────────────────────────╯
       ▼                             ╰──────────────╯ ╰─────╯
╭────────────────────────────╮        ╭────────────────────────────╮
│ login commands             │        │ provider implementations    │
│ openai / anthropic /       │        │ Codex / Messages / Gemini   │
│ antigravity                │        ╰────────────────────────────╯
╰────────────────────────────╯

Nexus is provider- and UI-neutral: it drives turns and approval policy, consumes
the provider's async `Stream<ProviderEvent>`, and renders nothing itself. It runs
on a tokio current-thread runtime (owned by the Tier-3 session driver) with a
per-turn `CancellationToken`: provider stream reads, tool futures, and approval
reviews are raced against cancellation via `tokio::select!`. Terminal I/O lives
behind front-end seams: the raw-mode terminal-surface TUI is used for interactive
TTYs, and the text UI remains the fallback for pipes/CI or TUI startup failure.

## Key Modules

| Module | Purpose | Public/internal API | Dependencies |
|---|---|---|---|
| `src/main.rs` | CLI entrypoint. Initializes telemetry/signals, parses args, materializes default prompt fragments, assembles the harness-owned system prompt, constructs the bare agent + startup-selected Mimir provider + tools, wraps them in a Tier-2 `wayland::Harness` (with optional session log, output store, and context budget), runs a new session, a resumed session (`resume <session-id>`: loads via `SessionStore::find`, seeds `Agent::resumed`, reopens the same log via `SessionLog::resume`, errors clearly on an unknown id), provider login commands, or `iris update`, and maps typed errors to process exit codes. `defaultProvider` supports `openai-codex`, `anthropic`, and `antigravity`; unset defaults to `openai-codex`. | `main()`, `dispatch()`, `run_agent()`, `resume_agent()`, `build_provider()`, `update_agent()` | `cli`, `nexus::Agent`, `wayland::{Harness, system_prompt}`, `session::{SessionLog, SessionStore}`, Mimir provider/auth/selection modules, `telemetry`, `errors` |
| `src/cli.rs` | Iris CLI session driver (Tier 3). Selects the terminal-surface TUI for interactive TTYs and falls back to `TextUi` for pipes/CI or TUI startup failure. Owns `ModelSwitch` state and shared `/model`/`/reasoning` text-command handling: safe-boundary provider rebuilds, reasoning validation/clamping, model-selection audit events, and no-op text feedback for TUI-only picker commands. The text path owns the current-thread runtime, reads prompts through `Ui`, exits through the slash-command registry, arms a per-turn `CancellationToken` with a background Ctrl-C watcher, and submits each turn on the `wayland::Harness` via `UiBridge`; the TUI path hands the same switch/harness state to `ui::tui_loop`. | `ModelSwitch`, `handle_model_command()`, `run_interactive()`, `run_session()` | `tokio`, `tokio_util::CancellationToken`, `config`, `mimir::{selection, model_capabilities}`, `wayland::Harness`, `nexus::ChatProvider`, `ui::{Ui, UiBridge, UiEvent, slash, tui, text}` |
| `src/nexus.rs` | Runtime core (Tier 1). A provider-, UI-, persistence-, and workspace-neutral async in-memory engine: owns conversation state + the injected `Tools` + approval policy, consumes the provider's `Stream<ProviderEvent>`, mints Nexus-owned `provider_turn_id`s for each provider/model round trip, enforces approval before gated tools, executes async tools against an injected `&ToolEnv` (consecutive concurrency-safe ungated calls run in parallel with bounded ordered buffering; everything else exclusively), and emits `AgentEvent`s to an `AgentObserver`; gates tools via `ApprovalGate`. Owns the provider-neutral `ToolOutput` and `ToolResultContract` serialization rules for success/error/denied/cancelled results, including typed output-handle metadata, while concrete tools own local details. The event stream includes display events plus provider-turn lifecycle, metadata-only tool lifecycle, output-handle, and compaction observability events correlated with ADR-0019 ids. Every emitted tool call gets a real or synthetic cancelled/denied result so the transcript stays valid on abort. Plans the model-visible tool surface from provider capabilities while preserving the full execution registry, and offloads oversized successful outputs through an injected `ToolOutputStore` when present. Can be seeded from a prior transcript on resume (`Agent::resumed`, which repairs a dangling trailing tool call so the rebuilt context is provider-valid). Holds no filesystem or session store. Bounds the tool loop and ends gracefully at the cap. | `ChatProvider` (`respond_stream`), `ProviderEvent`, `ProviderStream`, `ProviderCapabilities`, `Agent`, `Agent::submit_turn()`, `Agent::resumed()`, `Agent::messages()`, `Tool` (async `execute`, `is_concurrency_safe`), `Tools`, `ToolEnv`, `ToolOutputStore`, `ToolFuture`, `ToolOutput`, `AgentEvent`, `AgentObserver`, `ApprovalGate`, `ApprovalDecision`, `AssistantTurn`, `ToolCall`, `Message`, `Role` | `anyhow`, `serde_json`, `tracing`, `tokio`, `tokio_util::CancellationToken`, `futures`, `crate::tools` |
| `src/wayland/mod.rs` | Tier-2 harness. Wraps the bare `nexus::Agent`, owns the execution surface (workspace + `tools::ToolState`), optional `session::SessionLog`, context budget, message-entry cursor, and session-scoped output handle store. It compacts over-budget context at safe turn boundaries, emits typed compaction metadata through the Nexus observer seam, injects `ToolEnv` into each turn, records runtime model-selection audit entries, and persists new transcript messages post-turn (best-effort, diffing `agent.messages()`). `Harness::resumed` starts the persisted cursor past the loaded history so a resumed session continues the same log without rewriting it. Mirrors pi's `AgentHarness`. | `Harness`, `Harness::new()`, `Harness::resumed()`, `Harness::submit_turn()`, `Harness::record_selection_event()` | `anyhow`, `tracing`, `crate::{handles, nexus, session, tools}` |
| `src/wayland/system_prompt/mod.rs` | Harness-owned fragment/slot system prompt assembly. Materialized defaults live in `~/.iris/fragments`, repo fragments live in `<workspace>/.iris/fragments`, project docs (`AGENTS.md`/`CLAUDE.md`) are discovered upward, and generated live-tool blocks are appended. Fragment frontmatter supports `name` and numeric `slot` (`0` disables); unknown future selector keys are ignored. Every folded file is read through bounded, symlink-refusing IO. Fresh and resumed sessions call the same assembly path. | `assemble()`, `assemble_defaults()`, `ensure_default_fragments()` | `crate::{nexus::Tools, tools::path}`, filesystem APIs |
| `src/wayland/system_prompt/defaults.rs` | Shipped default prompt fragments and metadata, materialized on startup when absent and used as an in-memory fallback when no fragment files exist. Generated tool blocks are intentionally excluded from this data file. | `DEFAULTS` | none |
| `src/handles.rs` | Tier-2 session-scoped store for oversized tool outputs. Derives a `<session>.outputs/` sidecar directory, stores full output under truncated SHA-256 handles, validates handle ids on read, and implements Nexus's `ToolOutputStore` contract. | `HandleStore`, `HandleStore::for_session()`, `HandleStore::get()` | `sha2`, filesystem APIs, `crate::nexus::ToolOutputStore` |
| `src/ui/mod.rs` | Terminal front-end seam (Tier 3). Defines the `Ui` trait, the `UiEvent` render protocol, turn-error classification, and `UiBridge` (adapts a `Ui` onto the Nexus `AgentObserver`/`ApprovalGate` seams via `RefCell`). Shared event mapping keeps the TUI and text fallback consistent. | `Ui`, `UiEvent`, `UiBridge`, `TurnErrorKind` | `anyhow`, `crate::{nexus, errors}` |
| `src/ui/text.rs` | Text terminal fallback. Owns stdin/stdout/stderr, prints the `iris>` prompt, supports bracketed-paste/backslash multiline input, renders streamed assistant deltas and final tool lifecycle lines via `tool_display`, ignores live TUI-only deltas, prompts for approval, and routes auth/provider errors to stderr. Used for pipes/CI or when the TUI cannot start. | `TextUi`, `TextUi::stdio()` | `std::io`, `crate::{approval, nexus, ui, tool_display}` |
| `src/ui/slash.rs` | Slash-command registry and palette filtering. Registers only backed commands (`/exit`, `/quit`, `/model`, `/reasoning`, `/scoped-models`, `/settings`, `/login`, `/logout`) and provides shared matching/action helpers for TUI and text paths. | `COMMANDS`, `matches()`, `is_exit()`, `Palette`, `SlashAction` | none |
| `src/ui/markdown.rs` | Minimal pulldown-cmark to Ratatui `Line` renderer for assistant text. Covers headings, emphasis, inline/fenced code, lists, blockquotes, rules, and literal raw/inline HTML; tables/links/images/syntax highlighting remain out of scope. | `render_markdown()` | `pulldown_cmark`, `ratatui` |
| `src/ui/modal.rs` | Reusable modal state machines for provider/model, scoped-models, settings/effort, and login/logout selectors. Produces neutral `ModalOutcome`/`ModalAction` values for the TUI loop to execute. | `Modal`, `ModalOutcome`, selector structs | `ratatui`, `mimir::selection`, `ui::selector` |
| `src/ui/selector.rs` | Shared selector-list primitives used by TUI modals: filterable rows, selection movement, toggles, badges, and footer/status text. | `Selector`, `SelectorItem` | `ratatui` |
| `src/ui/picker.rs` | Model/reasoning picker helpers. Builds authenticated model lists from the Mimir catalog, applies exact-match and scoped-cycle behavior, cycles reasoning effort, and persists default/scoped settings when requested. | picker builders, `cycle_model()`, `cycle_effort()` | `config`, `mimir::{model_catalog, model_capabilities, selection}`, `ui::modal` |
| `src/ui/login.rs` | `/login` and `/logout` orchestration for the TUI. Builds provider/auth-status selectors, removes stored credentials only, and runs existing blocking OAuth helpers behind a `LoginBackend` seam. | `open_login()`, `provider_select()`, `open_logout()`, `apply_logout()`, `LoginBackend` | `reqwest`, `mimir::auth`, `mimir::model_catalog`, `ui::modal` |
| `src/ui/terminal_surface.rs` | Iris-owned terminal surface renderer. Converts Ratatui `Line`s to ANSI, tracks previous rendered lines and terminal size, writes synchronized output, appends/diffs safe changes, and performs full state replay on resize or unsafe shrink without using Ratatui `Terminal`/inline viewport lifecycle. Width/clip math is routed through `ui::textengine`. | `TerminalSurface`, `RenderState`, `RenderKind`, `RenderStats` | `ratatui`, `crate::ui::textengine`, `std::io` |
| `src/ui/textengine.rs` | Unified text engine: the single source of truth for display width, ANSI/OSC/APC parsing, and width-aware wrap/truncate/slice. Grapheme-cluster width (CJK/emoji ZWJ/VS16/flag/combining), one ANSI/OSC/APC stripper (`strip_ansi`/`clean_text`/`visible_width`), grapheme-safe `truncate_chars`/`truncate_to_width`/`wrap_to_width`, and a reserved `ansi_aware` subsystem (SGR carry + OSC 8 hyperlink preservation across wrapped/truncated lines) for a deferred hyperlink UI feature. Replaces the former divergent helpers in `tui/wrap.rs`, `tui/text.rs`, and `ui/text.rs` (including a `chars().count()` width bug). | `display_width`, `visible_width`, `strip_ansi`, `clean_text`, `truncate_to_width`, `wrap_to_width`, `clip_to_width`, `ansi_aware::{wrap_ansi, truncate_ansi, slice_by_column}` | `unicode-width`, `unicode-segmentation` |
| `src/ui/tui.rs` | Terminal-surface TUI state and document rendering. Owns raw mode/paste/key flags/cursor visibility through Crossterm, keeps transcript history in `Screen` state for replay, renders transcript plus textarea editor, spinner state, slash palette, modals, approval display, live exec cells (`ToolStarted`/`ToolOutputDelta`), Markdown, and width-aware layout into Ratatui `Line`s. Reads no input itself. | `TuiUi`, `Screen` | `ratatui`, `ratatui_textarea`, `crate::{approval, tool_display, ui}` |
| `src/ui/tui_loop.rs` | Async event loop for the terminal-surface TUI. Multiplexes terminal input, render ticks, agent events, active-turn cancellation, approval requests, slash/modal actions, runtime provider/model/reasoning switches, scoped model cycling, and TUI login flows on the current-thread runtime; Ctrl-C in raw mode cancels the active turn from the input thread. | `run()` | `tokio`, `ratatui::crossterm`, `tokio_util::CancellationToken`, `wayland::Harness`, `nexus` seams, `cli::ModelSwitch`, UI modal/login/picker modules |
| `src/tool_display.rs` | Presentation-only formatter for tool-call lines (proposed/approval/denied/result/error). Returns owned strings, performs no I/O, and never changes what is sent to the model. | `summarize()`, `proposed_line()`, `approval_prompt()`, `denied_line()`, `result_line()`, `error_line()` | `serde_json`, `crate::nexus::ToolCall` |
| `src/approval.rs` | Terminal decision parser (Tier 3). Translates a typed line into the Tier-1 `crate::nexus::ApprovalDecision`: `y`/`yes` allow, `a`/`always` allow-session, anything else denies. | `parse_decision()` | `crate::nexus` |
| `src/errors.rs` | Provider-neutral typed errors carried across runtime boundaries for user-facing handling and exit codes. | `AuthError`, `UsageError`, `exit_code()` | `thiserror` |
| `src/telemetry.rs` | Operator observability: `RUST_LOG`-driven tracing to stderr, secret-safe fingerprints, and sanitization of external response bodies before they reach logs/errors. | `init()`, `redact_secret()`, `sanitize_external_body()` | `tracing-subscriber`, `sha2`, `serde_json` |
| `src/config.rs` | Iris settings file loader + updater. Reads `~/.iris/settings.json` (global) and `<cwd>/.iris/settings.json` (project). Project config may override `defaultModel`, `defaultReasoning`, and `contextTokenBudget`; global/user config owns `defaultProvider`, `baseUrl`, `promptCacheRetention`, `anthropicContextManagement`, and `enabledModels` so repo-local settings cannot redirect bearer tokens, change provider cycles, extend provider-side cache retention, or enable server-side context edits. Unknown keys are ignored; malformed JSON errors loudly; global updates are written atomically while preserving unknown keys. | `Settings`, `Settings::load()`, `Settings::context_token_budget()`, `save_default_model()`, `save_default_reasoning()`, `save_enabled_models()`, `default_model_qualified()` | `serde`, `serde_json`, `anyhow`, env/filesystem APIs |
| `src/session.rs` | JSONL session store + linear resume + compaction-aware rebuild. `SessionLog` (write) appends a `session` header then tree-ready `message` lines with stable `id`, `parentId`, `tokenEstimate`, and optional `providerTurnId`, plus `compaction` entries covering inclusive message-id ranges and `modelSelection` audit entries for runtime provider/model/reasoning switches. Assistant-reasoning rows persist provider-origin metadata for replay where supported. `SessionLog::resume` reopens an existing transcript for append, restoring the leaf link + id counter and terminating a truncated final fragment. `SessionStore` (read) lists sessions with cheap header+mtime metadata (`list()`, newest-first), resolves one by id (`find()`), and opens one back (`open()`), replacing covered ranges with summary messages and returning rebuilt context token totals. No transcript-tree branching/rollback or in-session `/resume` picker UI yet. | `SessionLog` (`create`/`resume`/`append`/`append_compaction`/`append_model_selection`/`id`/`path`), `SessionStore` (`open_default`/`with_root`/`list`/`find`/`open`), `SessionMeta`, `StoredSession`, `estimate_tokens()` | `serde_json`, `anyhow`, `rand`, filesystem/time APIs, `crate::nexus::{Message, Role}` |
| `src/signals.rs` | Graceful SIGINT handling for the REPL. First Ctrl-C sets an interrupt flag the tool loop checks between round-trips (ends the turn cleanly); a second reaps tracked process groups via `process_group`, restores the default handler, and re-raises to force-quit. | `install()`, `interrupted()`, `reset()` | `libc`, `crate::process_group`, atomics |
| `src/process_group.rs` | Single owner of process-group spawn/kill/reap policy for `bash` shells. Puts commands in their own group, kills+reaps groups, and keeps a lock-free registry so the force-quit SIGINT handler can SIGKILL every live group with only async-signal-safe ops. | `in_own_group()`, `kill()`, `kill_and_reap()`, `register()`, `kill_all_from_signal()`, `GroupGuard` | `libc`, atomics, `std::process` |
| `src/tools/mod.rs` | Built-in tool module root. Declares the per-tool modules, owns `ToolState` (observed files + bash sessions) injected via `ToolEnv`, re-exports the Tier-1 `ToolOutput` contract and `built_in_tools()`, and provides shared diff/preview rendering (`Preview`, `render_preview`, `unified_diff`). Per-tool modules fill model-facing `content` plus bounded host metadata; Nexus serializes the enclosing result envelope. | `ToolState`, `ToolOutput` (re-export), `built_in_tools` (re-export), `Preview` | tool submodules, `crate::nexus::ToolOutput`, `similar`, `anyhow` |
| `src/tools/registry.rs` | Built-in tool adapters (Tier 3). One thin `Tool` impl per tool (`ReadTool`…`LsTool`) wrapping the per-tool `execute`/`parameters` functions plus self-classification (`requires_approval`, `is_destructive`, `is_concurrency_safe`, `diff_preview`); `grep`/`find`/`ls` run their blocking body on `spawn_blocking` and are concurrency-safe, `read`/`edit`/`write`/`bash` stay exclusive. `built_in_tools()` is the injection point the CLI passes into the agent. | `built_in_tools()`, `Tool` impls | `crate::nexus::{Tool, ToolEnv, ToolFuture, ToolOutput, Tools}`, `tokio`, `tokio_util::CancellationToken`, tool submodules |
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
| `src/mimir/mod.rs` | Mimir module declaration: Iris's AI/provider package (the pi-ai equivalent), housing provider adapters, auth, model selection, catalog, and capability metadata. The `ChatProvider` contract stays in `nexus`. See [`../NAMING.md`](../NAMING.md). | `auth`, `providers`, `selection`, `model_catalog`, `model_capabilities`, `anthropic_models` modules | mimir submodules |
| `src/mimir/selection.rs` | Normalized provider/model/reasoning/cache/context-management selection and precedence. Parses supported provider ids, `ReasoningEffort`, and global-only `PromptCacheRetention` (`none`/`short`/`long`); resolves `env > settings > defaults` for model/base-url; validates supported Anthropic `ContextManagement` clear edits while rejecting provider compact replay; and centralizes per-provider defaults. | `ProviderId`, `ReasoningEffort`, `PromptCacheRetention`, `ContextManagement`, `ModelSelection`, `base_url_for()` | `config`, `errors`, env APIs |
| `src/mimir/model_capabilities.rs` | Reasoning capability table and clamp/validation logic for supported provider/model pairs. Drives startup validation, `/reasoning`, effort picker, and Shift+Tab cycling. | `supported_levels()`, `supports_thinking()`, `cycle_effort()`, `validate()`, `clamp()` | `errors`, `mimir::{selection, anthropic_models}` |
| `src/mimir/model_catalog.rs` | Hand-maintained TUI model catalog and no-secret auth availability view. Filters picker candidates to authenticated providers, hides Fable 5 unless opted in, and provides display/context-window labels and exact-match resolution. | `CatalogModel`, `AuthStatus`, `all()`, `available_models()`, `provider_status()`, `exact_match()` | `mimir::{auth, selection}` |
| `src/mimir/anthropic_models.rs` | Claude Code subscription model matrix: model ids, output caps, manual/adaptive thinking mode, and refusal fallback. Used by Anthropic request construction, catalog sync tests, and reasoning capability checks. | `AnthropicModel`, `ThinkingMode`, `MODELS`, `find()`, `is_subscription_model()` | none |
| `src/mimir/auth/mod.rs` | Auth module declaration. | `anthropic`, `antigravity`, `device_code`, `openai_codex`, `storage` modules | auth submodules |
| `src/mimir/auth/storage.rs` | Provider-keyed auth-file storage for OAuth credentials. Reads missing files as empty, validates credential shape, reports stored credential kinds for `/login`/`/logout`, removes individual provider records, and writes atomically with restricted Unix permissions. | `AuthStore`, `OAuthCredentials`, `StoredCredential`, `CredentialKind` | filesystem/env APIs, `anyhow`, `serde`, `serde_json` |
| `src/mimir/auth/device_code.rs` | Generic polling helper for OAuth device-code flows. | `DeviceCodePoll`, `poll_device_code()` | `std::thread`, `std::time`, `anyhow` |
| `src/mimir/auth/oauth_callback.rs` | Shared provider-local OAuth browser-login plumbing: PKCE S256, cancel-aware loopback callback server (IPv4 plus best-effort IPv6), manual code/redirect paste parsing, timeout/error callback classification, and safe callback response rendering. | callback helpers and input parsers | TCP/time APIs, `sha2`, `rand`, `anyhow` |
| `src/mimir/auth/openai_codex.rs` | OpenAI Codex OAuth integration. Supports browser callback login through the shared callback seam, device-code login, token exchange/refresh, and account ID extraction from JWT payloads. | `OpenAiCodexTokenStore`, `AccessToken`, `login_browser()`, `login_device_code()` | `AuthStore`, `oauth_callback`, `poll_device_code`, `base64`, `rand`, `reqwest`, `sha2`, `serde`, `serde_json`, TCP/filesystem/time APIs |
| `src/mimir/auth/anthropic.rs` | Anthropic Claude Code subscription OAuth integration. Runs browser PKCE login with manual paste fallback, loads credentials from the Iris auth store or bootstraps from Claude Code's `.credentials.json`, supports macOS Claude Code Keychain parity checks/write-back, refreshes via Anthropic OAuth, and writes rotated tokens back to the same source without reshaping/dropping sibling keys. | `AnthropicTokenStore`, `login_browser()`, `AUTH_PROVIDER` | `AuthStore`, `oauth_callback`, filesystem/env APIs, `reqwest`, `serde_json`, `anyhow` |
| `src/mimir/auth/antigravity.rs` | Antigravity Google OAuth integration. Runs browser PKCE login on `127.0.0.1:51121`, uses a runtime or build-time `ANTIGRAVITY_CLIENT_SECRET` for login/refresh, decodes the public installed-app client ID at runtime, refreshes tokens, and discovers/persists `projectId` via Code Assist (`ANTIGRAVITY_PROJECT_ID` wins over persisted ids; no hard-coded fallback is persisted on discovery failure). | `AntigravityTokenStore`, `login_browser()`, `AUTH_PROVIDER` | `AuthStore`, `base64`, `rand`, `reqwest`, `sha2`, `serde_json`, TCP/filesystem/time APIs |
| `src/mimir/providers/mod.rs` | Provider module declaration plus shared prompt-cache stable-prefix diagnostics. System-prompt assembly lives in `wayland/system_prompt.rs`; provider constructors receive the assembled prompt from `main.rs`. | `anthropic_messages`, `antigravity`, `openai_codex_responses` modules, `PromptCachePrefix` | provider submodules, Nexus message/tool types |
| `src/mimir/providers/transport.rs` | Shared blocking-provider glue: spawns reqwest/SSE work on `spawn_blocking`, forwards events over a channel, classifies HTTP status, performs exactly-once reauth, and parses SSE event framing. | `TurnSink`, `ChannelSink`, `spawn_stream()`, `run_with_reauth()`, `for_each_sse_event()`, `classify_http_status()` | `futures`, `tokio`, `tokio_util`, `reqwest`, `anyhow`, Nexus turn/event types |
| `src/mimir/providers/openai_codex_responses.rs` | Implements `ChatProvider::respond_stream` for the ChatGPT Codex Responses endpoint. Runs blocking reqwest/SSE code through the shared transport, forwards text deltas/completion metadata and the final turn as `ProviderEvent`s, builds request JSON/headers/URL, advertises tools, maps normalized reasoning to Responses `reasoning.effort`, emits default-off `prompt_cache_key`/24h retention hints, parses usage/cache/reasoning-token metadata, retries with backoff, and is cancellation-aware. Assistant-reasoning rows are filtered out on this lane today. | `OpenAiCodexResponsesProvider` | `OpenAiCodexTokenStore`, shared transport, `ChatProvider`, Nexus message/turn types, `mimir::selection`, `crate::{tools, errors, telemetry}`, `reqwest`, `serde_json`, `tracing` |
| `src/mimir/providers/anthropic_messages.rs` | Implements `ChatProvider::respond_stream` for Anthropic Messages on the Claude Code OAuth lane. Builds Claude Code identity/system blocks, enforces user/assistant role alternation, advertises tools, maps normalized reasoning to manual-budget or adaptive thinking per `anthropic_models`, replays same-origin signed/redacted reasoning blocks, applies default-off `cache_control` markers and supported `context_management` clear edits, parses Anthropic SSE text/reasoning/tool-call/usage/cache blocks, redacts external diagnostics, and reauths once on auth rejection. | `AnthropicProvider` | `AnthropicTokenStore`, shared transport, `ChatProvider`, Nexus message/turn/reasoning types, `mimir::{anthropic_models, selection}`, `reqwest`, `serde_json`, `tracing` |
| `src/mimir/providers/antigravity.rs` | Implements `ChatProvider::respond_stream` for Antigravity/Gemini Code Assist (`v1internal:streamGenerateContent?alt=sse`). Builds the project/model/request envelope, maps Nexus messages/tools to Gemini contents/function declarations, maps normalized reasoning to `generationConfig.thinkingConfig`, captures and replays Gemini tool-call `thoughtSignature` continuity, parses SSE response chunks/text/function calls, and reauths once on auth rejection. Assistant-reasoning rows are skipped on this lane today. | `AntigravityProvider` | `AntigravityTokenStore`, shared transport, `ChatProvider`, Nexus message/turn types, `mimir::selection`, `reqwest`, `serde_json`, `tracing` |

## Data Flow

1. `main()` calls `telemetry::init()`, installs the SIGINT handler via `signals::install()`, and runs `dispatch()`.
2. For the default command, `run_agent()` loads `config::Settings` for the cwd, materializes default prompt fragments, assembles the Wayland-owned system prompt from fragments + project docs + live tools, resolves `defaultProvider`/`defaultModel`/`defaultReasoning`/`promptCacheRetention`/Anthropic context-management config from global/project config and env (unset/blank provider → `openai-codex`; supported: `openai-codex`, `anthropic`, `antigravity`), validates reasoning capabilities, builds the selected Mimir provider, creates an `Agent`, attaches a best-effort `session::SessionLog` (warns and continues in-memory if it cannot be opened), passes the configured context token budget into the harness, initializes `cli::ModelSwitch` with any global `enabledModels`, and calls `cli::run_interactive()`.
3. `run_interactive()` selects the terminal-surface TUI when stdin/stdout are terminals, otherwise the text fallback. The text fallback uses `run_session()` to create the tokio current-thread runtime, emit `SessionStarted`, loop over `Ui::next_prompt()`, skip blanks, break on slash exit commands, apply `/model` and `/reasoning` at safe turn boundaries, treat TUI-only picker commands as status no-ops, arm a per-turn `CancellationToken` plus a Ctrl-C watcher thread, and `block_on(Harness::submit_turn(prompt, observer, gate, token))`. The TUI path runs `ui::tui_loop::run()` on the same runtime shape and multiplexes input/render/turn/approval channels, slash/modals, scoped model cycling, login/logout, and runtime provider/reasoning switches.
4. `Harness::submit_turn()` first auto-compacts at a safe turn boundary when the current context exceeds the budget, then injects `ToolEnv` (workspace, `ToolState`, optional output store) into `Agent::submit_turn()`, which appends `Message::user(prompt)` and runs `complete_turn()`.
5. `complete_turn()` calls `ChatProvider::respond_stream(messages, tools, token)`, which returns a `Stream<Result<ProviderEvent>>`; the loop races each stream read against the turn token via `tokio::select!`.
6. The selected Mimir provider runs blocking HTTP/SSE work on `spawn_blocking` through `transport.rs`: it reads or refreshes provider credentials, converts Nexus messages/tools to that provider's wire JSON, applies normalized reasoning/thinking fields where supported, applies default-off cache/context-management hints where configured, sends a cancellation-aware request (with retry/backoff or one-shot reauth where implemented), and forwards parsed events onto a `futures` channel as `ProviderEvent::TextDelta` / `ProviderEvent::Completed` with usage/cache metadata where available.
7. Nexus emits `AssistantText`/`AssistantTextEnd` for deltas/final text and appends the final assistant turn to conversation state.
8. With no tool calls, Nexus emits `TurnComplete` and returns.
9. Tool calls run via `run_tools()`: consecutive concurrency-safe, ungated calls form a bounded parallel batch (in-order results); every other call runs exclusively. For each call Nexus records the assistant tool call. Gated tools (`Tool::requires_approval()`) emit a `DiffPreview` when `Tool::diff_preview()` returns one, then `ApprovalGate::review()` collects a decision (raced against cancellation); denial emits `ToolDenied` and records `{ ok: false, denied: true }`. Ungated tools emit `ToolProposed`.
10. Allowed or ungated calls run `Tool::execute()` (a future given a child token, raced against cancellation); one-shot bash also streams display-only chunks through `ToolOutputSink` as `ToolOutputDelta` while accumulating the final output. Nexus emits the full/final output to the UI as `ToolResult`/`ToolError` (with exit metadata where present) and records model-facing JSON. Successful outputs over 16 KiB are replaced in the transcript by a compact preview plus `outputHandle` metadata when an output store is attached; otherwise the full output stays inline. On cancellation every emitted call still gets a real or synthetic cancelled/denied result so the next request stays valid.
11. The loop repeats until the assistant returns no tool calls or the bounded `MAX_TOOL_ROUNDTRIPS` cap is hit, at which point Nexus emits a `Notice` and `TurnComplete` (graceful, not an error). A tripped turn token ends the turn promptly and returns to the prompt.
12. When a session log is attached, the harness appends newly committed messages to the JSONL transcript after the turn returns, even on turn error. On `resume <id>`, `resume_agent()` loads the target session (`SessionStore::find` + `open`), whose read path already rebuilt through compaction summaries, seeds the agent with those messages (`Agent::resumed`), reopens the same file for append (`SessionLog::resume`), and starts the harness persisted cursor past the loaded history so continued turns extend the same log without rewriting it.
13. Turn errors from `submit_turn()` are classified by `UiEvent::from_turn_error()` into auth vs provider and rendered to stderr; the session continues.

## Configuration and Inputs

| Input | Default | Used by |
|---|---|---|
| `~/.iris/settings.json`, `<cwd>/.iris/settings.json` | absent (built-in defaults) | `config::Settings::load()` (project overrides `defaultModel`, `defaultReasoning`, and `contextTokenBudget`; global owns provider/base-url, `promptCacheRetention`, `anthropicContextManagement`, and `enabledModels`) |
| `~/.iris/fragments`, `<cwd>/.iris/fragments` | shipped defaults when no fragments exist | `wayland::system_prompt` fragment assembly (`name`, numeric `slot`, `slot: 0` disabled) |
| `IRIS_AUTH_PATH` | `~/.iris/auth.json` | Mimir token stores (`AuthStore::from_env()`) |
| `IRIS_CONFIG_PATH` | `~/.iris/settings.json` | global settings path override (`config::Settings`) |
| `IRIS_SESSION_DIR` | `~/.iris/sessions` | transcript root (`session::SessionLog`) |
| `IRIS_MODEL` | `gpt-5.5` | OpenAI Codex model override (`env > settings > default`) |
| `IRIS_CODEX_BASE_URL` | `https://chatgpt.com/backend-api` | OpenAI Codex base-url override (`env > settings > default`) |
| `CLAUDE_CONFIG_DIR` | `~/.claude` | Anthropic credential bootstrap path (`CLAUDE_CONFIG_DIR/.credentials.json`) |
| `ANTIGRAVITY_CLIENT_SECRET` | none unless injected at build time | Antigravity Google OAuth token exchange/refresh |
| `ANTIGRAVITY_PROJECT_ID` | discovered project | Antigravity project-id override (wins over stored `projectId`) before `loadCodeAssist` discovery |
| `IRIS_ENABLE_FABLE_5` | unset / hidden | model catalog opt-in for surfacing gated `claude-fable-5` in `/model` |
| `RUST_LOG` | `warn` | `telemetry::init()` tracing filter |
| `HOME` | required when the matching path override is unset | auth/settings/session path resolution |

## CLI Commands

| Command | Purpose |
|---|---|
| `iris` | Start a new interactive agent session in the current working directory. |
| `iris resume <session-id>` | Resume a prior session by id: load its transcript, rebuild provider-visible context, and continue appending future turns to the same log. Errors with exit code `2` on an unknown id. |
| `iris login openai-codex` | Run browser OAuth login using a local callback server. |
| `iris login openai-codex --browser` | Explicit browser OAuth login. |
| `iris login openai-codex --device-code` | Run device-code OAuth login. |
| `iris login anthropic` | Run Anthropic browser PKCE OAuth with manual paste fallback; Iris can also reuse a Claude Code OAuth token when present. |
| `iris login antigravity` | Run Google browser PKCE OAuth login (requires `ANTIGRAVITY_CLIENT_SECRET` unless the binary was built with it). |
| `iris update` | Update the installed binary from the GitHub repository via locked Cargo install. |
| `iris help` / `--help` / `-h` | Print command help. |

Interactive slash commands: `/exit`, `/quit`, `/model`, `/reasoning`,
`/scoped-models`, `/settings`, `/login`, and `/logout`. The text fallback
executes `/model` and `/reasoning`, exits on `/exit`/`/quit`, and treats the
TUI-only modal commands as status no-ops.

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
- `base64` — base64url JWT payload decoding, OAuth PKCE/client-id decoding, and auth-file secret encoding.
- `futures` — `Stream` trait and the unbounded channel bridging the provider's `spawn_blocking` task to the async loop.
- `pulldown-cmark` — assistant Markdown parsing for the TUI transcript renderer.
- `ratatui` / `ratatui-textarea` — TUI text/style/layout primitives, modal selectors, and textarea editing; Iris owns the production terminal surface diff/replay.
- `tokio` — current-thread async runtime, `spawn_blocking`, and `tokio::select!` cancellation races.
- `tokio-util` — `CancellationToken` for per-turn and per-tool cancellation.
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

- Session loop, conversation persistence, streamed-delta rendering, TUI/text front-end behavior, slash palette/input handling, modal/model/login selectors, runtime model/reasoning switching, and auth/provider error recovery in `src/nexus.rs`, `src/cli.rs`, and `src/ui/`.
- Tool-call loop execution, graceful round-trip limiting, diff-preview-before-approval ordering, tool error encoding, approval allow/deny handling, and workspace path/symlink rejection in `src/nexus.rs`.
- Terminal decision parsing in `src/approval.rs`.
- Typed-error exit-code classification (including through `context` wrapping) in `src/errors.rs`.
- Secret redaction and external-body sanitization in `src/telemetry.rs`.
- Tool-call display formatting in `src/tool_display.rs`.
- Settings file loading/merge precedence, context-token-budget parsing, global-only prompt-cache/context-management controls, unknown-key tolerance, and malformed-file errors in `src/config.rs`.
- JSONL transcript header/append/tool-call entries, assistant-reasoning rows, model-selection audit entries, cwd slugging, by-id `find`, token estimates, compaction entries/rebuild, and resume (same-log append with linked ids, resume after a truncated fragment) in `src/session.rs`.
- Session resume end-to-end (rebuilt prior context reaches the next model turn without duplicating history), dangling-tool-call repair, provider-specific tool-surface planning, large-output handles, and auto-compaction in `src/nexus_tests.rs`.
- Session-scoped handle storage in `src/handles.rs`.
- Harness-owned fragment/default/project-doc system-prompt assembly in `src/wayland/system_prompt/`.
- SIGINT first-press/repeat flag behavior in `src/signals.rs`.
- Process-group registration/guard, targeted kill, and backgrounded-grandchild reaping in `src/process_group.rs`.
- Built-in tool behavior under `src/tools/`, including read/write/edit, atomic writes, `ls`, optional `grep`/`find` integration, bash output/timeout/process-group handling, persistent sessions, background jobs, Landlock sandbox decision/fallback, diff previews, and dispatch/tool-definition coverage.
- Larger Nexus and Codex-provider suites split into `src/nexus_tests.rs` and `src/mimir/providers/openai_codex_responses_tests.rs`.
- Auth storage parsing and atomic restricted writes in `src/mimir/auth/storage.rs`.
- Device-code polling behavior in `src/mimir/auth/device_code.rs`.
- Shared OAuth callback parsing/cancellation/manual-paste behavior in `src/mimir/auth/oauth_callback.rs`.
- JWT account extraction, browser OAuth URL/callback parsing, device-code interval parsing, and device-auth error parsing in `src/mimir/auth/openai_codex.rs`.
- Anthropic Claude Code credential parsing/write-back, browser login, refresh response parsing, subscription model matrix, reasoning request construction/replay, cache/context-management request construction, role alternation, request construction, and Anthropic SSE text/reasoning/tool-call/usage parsing.
- Antigravity PKCE URL/callback parsing, runtime/build-time client-secret resolution, project-id discovery helpers, request construction, thinking config, tool schema sanitization, Gemini `thoughtSignature` replay, and Gemini SSE text/tool-call parsing.
- Shared provider transport behavior: SSE framing, exactly-once reauth, HTTP-status classification, and dropped-stream handling.
- Codex URL resolution, request JSON construction, streamed text/delta parsing, tool-call parsing, and missing-output errors in `src/mimir/providers/openai_codex_responses.rs`.

## Known Gaps

Milestone 1, the async-hard runtime, and the Milestone 2 foundations are
complete: `ChatProvider` is an async streaming contract, each turn owns a
`CancellationToken`, provider reads / tool futures / approval reviews are raced
against cancellation, tools receive child tokens, concurrency-safe tools run in
parallel, large outputs are handle-backed, token estimates persist, and the
harness auto-compacts at turn boundaries. Documented runtime caveats (see
[`../ROADMAP.md`](../ROADMAP.md)): the real terminal approval prompt is a
blocking stdin read, so the first Ctrl-C cannot preempt a *pending* prompt; an
idle provider socket read and an abandoned `grep`/`find`/`ls` walk are not
force-aborted mid-flight. Linear session resume is implemented (`resume <id>`
rebuilds prior context and continues the same log), as are runtime model and
reasoning switches. The remaining Milestone 2 gate is proof: benchmark that the
handle/token/compaction path reduces prompt tokens without reducing task success.
Also missing: handle dereference UI/tool, richer context planner/ledger,
provider-quality summaries, provider-side compact replay, in-session `/resume`
picker UI, transcript-tree branching/rollback, persistent approval policies,
modes, subagents, and git/GitHub workflow.

## Related Areas

- [`../ROADMAP.md`](../ROADMAP.md) — milestone sequencing and acceptance criteria.
- [`../FEATURES.md`](../FEATURES.md) — implemented/planned capability inventory.
- Project agent guidelines — local agent operating rules (not tracked as a repository doc).
