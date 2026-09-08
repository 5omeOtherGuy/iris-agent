# Iris Current Codemap

**Last checked:** 2026-09-08 against `b420985`.
**Entry points:** `src/main.rs` → `src/lib.rs::run_cli`, `iris-bench/src/main.rs`.

This map describes implemented modules and their boundaries. It is navigation,
not a claim that every code path was rerun. See [audit coverage](../DOCUMENTATION_AUDIT.md),
[capability status](../FEATURES.md), and [v1.0 gates](../ROADMAP.md).

## Workspace and dependency direction

```text
iris-agent: main.rs -> lib.rs -> cli.rs / print.rs / ui/
                                  |
                           wayland::Harness
                                  |
                            nexus::Agent <- injected Mimir providers + tools
                                  ^
                      neutral events and contracts

Wayland -> iris-subagent-runtime (scheduler, artifacts, managed worktrees)
iris-bench -> iris-agent::harness (benchmark facade)
```

`Cargo.toml` contains the root `iris-agent` package and members `iris-bench` and
`crates/iris-subagent-runtime`. Nexus/Wayland/Mimir/UI remain in-crate modules.
The broader [workspace split](../MODULARIZATION.md) is proposed, not implemented.

Nexus renders nothing and owns no session store. Its boundary is not fully
clean: `ToolEnv` references `crate::tools::ToolState`, and Nexus uses
`crate::tools::path::workspace_relative` and `crate::display_path::workspace_path`.
See [current vs target](../ARCHITECTURE.md#current-vs-target).

## Process, runtime and storage

Paths are repository-relative. API names are navigation hints, not a public SDK
stability promise.

| Module | Responsibility / seams | Main dependencies |
| --- | --- | --- |
| `src/main.rs` | Thin binary shim calling `iris_agent::run_cli()` | `iris-agent` library |
| `src/lib.rs` | CLI parsing/dispatch, settings/auth, provider/tool construction, print/resume/login/update startup | `cli`, `print`, `mimir`, `wayland`, `config` |
| `src/cli.rs` | Interactive session driver, `ModelSwitch`, shared command builders, text-path compaction/delegation controls | Nexus/Wayland contracts, `ui`, `mimir` |
| `src/print.rs` | Headless execution and optional usage report; no rich terminal dependency in output | harness, metrics, tools |
| `src/nexus.rs` | `Agent`, `ChatProvider`, `Message`, `AgentEvent`, `Tool`, `Tools`, `ToolEnv`, `ApprovalGate`, `ContextGovernor`; scheduling, approvals, cancellation, overflow rewrite and steering boundaries | Tokio/futures, neutral contracts; concrete exceptions above |
| `src/wayland/mod.rs` | `Harness`, execution/session ownership, contextual input, incremental persistence, `TurnContextController`, turn-edge compaction | Nexus, session, tools, settings |
| `src/config.rs`, `src/config/tool_result_compaction.rs` | Global/project settings, validation, precedence and atomic updates; typed reducer policy | serde, filesystem, provider selection |
| `src/session.rs` | `SessionLog`, `SessionStore`, JSONL entries, linear resume, folds/summaries/carry reconstruction, transport/task/goal audit data | Nexus values, filesystem APIs |
| `src/handles.rs` | `HandleStore`: session sidecars, validated content-addressed output ids, bounded retrieval | Nexus output-store contract, SHA-256 |
| `src/goal.rs`, `src/goal_tests.rs` | Durable goal state, budgets, accounting and continuation policy | session/harness integration |
| `src/metrics.rs` | Shared provider-usage, context and rate accounting | Nexus usage/events |
| `src/errors.rs`, `src/telemetry.rs` | Boundary errors/exit codes, tracing, secret-safe diagnostic helpers | thiserror, tracing, serde |
| `src/signals.rs`, `src/process_group.rs` | SIGINT state, force-quit cleanup, owned shell process-group kill/reap | atomics, libc |
| `src/selfupdate.rs` | Stable-release selection, checksum/self-replace for dist builds, Cargo fallback | HTTP/filesystem/process APIs |

## Context and delegated work

| Module | Responsibility / seams |
| --- | --- |
| `src/wayland/compaction.rs` | `CompactionEngine`: range planning, structured summary/carry, durable parent-owned validation and application |
| `src/wayland/compaction_governor.rs` | Mid-round-trip pressure governance and deterministic reactive overflow recovery |
| `src/wayland/compaction_background.rs` | One-job lifecycle, polling, async hard wait, native/portable/excerpt fallback; ready summaries remain held until hard pressure or manual drain |
| `src/wayland/trigger.rs` | `TriggerLadder`: model-aware warn/start/hard thresholds, retained tail, small-window deterministic policy |
| `src/wayland/fold.rs` | Recoverable spent-tool-result folding and cache-aware flush planning; default-off policy resolved by config |
| `src/wayland/structured_summary/` | Typed structured-summary validation and repair pipeline |
| `src/wayland/system_prompt/` | Compiled fragment assembly, generated live-tool blocks, bounded root-to-leaf project instructions and onboarding; no disk fragment loading |
| `src/wayland/skills/` | Bounded discovery, Codex metadata/config compatibility, progressive disclosure, invocation and resource grants |
| `src/wayland/trust.rs` | HOME-owned canonical-cwd project permission grants; project files cannot grant permissions |
| `src/wayland/git_safety/` | Dirty-tree attribution, checkpoints, ownership leases, net diff, settlement/recovery; mutation guard default-on, durable task workflow opt-in |
| `src/wayland/worker_runtime.rs` | Iris executor adapters for the shared scheduler; `!Send` provider/agent construction on scheduler thread |
| `src/wayland/subagents.rs` | `SubagentBackend`, `SubagentTypeManifest`, `ChildProviderFactory`; general/explore/review defaults, tool ceilings, strict worker paths, managed mutation worktrees and reviewed apply |
| `crates/iris-subagent-runtime/` | Host-neutral durable scheduling, cancellation, groups, artifacts, linked/Btrfs worktrees, leases/pooling/adoption/restore and immutable apply plans |

Runtime group APIs are implemented in the public worker crate. Iris's native
model-facing best-of-N controls were removed by ADR-0065; the adapter remains
dormant. A validated manifest child policy does not enable nested delegation.
Main-session path/shell confinement is opt-in; workers enforce stricter path and
shell rules. Neither a worktree nor an approval prompt is a general sandbox.

## Mimir — provider and authentication adapters

| Module | Responsibility / seams |
| --- | --- |
| `src/mimir/selection.rs` | `ModelSelection`, provider defaults, setting/env precedence, reasoning/cache/context policy and worker-route resolution |
| `src/mimir/model_catalog.rs` | Catalog, credential availability, authenticated worker lanes, OAuth preference and explicit model/lane selection |
| `src/mimir/model_capabilities.rs`, `src/mimir/anthropic_models.rs` | Model reasoning/window/capability metadata, supported levels and clamp/validation |
| `src/mimir/auth/storage.rs`, `src/mimir/auth/api_key.rs` | Restricted atomic credential storage, API-key resolution and lane separation |
| `src/mimir/auth/openai_codex.rs`, `src/mimir/auth/device_code.rs` | Codex OAuth browser/device flows and token refresh |
| `src/mimir/auth/anthropic.rs`, `src/mimir/auth/antigravity.rs` | Provider OAuth, credential reuse/write-back and project discovery |
| `src/mimir/auth/oauth_callback.rs` | Shared cancellable loopback PKCE/manual-paste plumbing |
| `src/mimir/providers/mod.rs`, `src/mimir/providers/transport.rs` | Adapter declarations, stable-prefix diagnostics, blocking transport bridge, SSE framing, reauth and status classification |
| `src/mimir/providers/openai_codex_responses.rs` | Codex Responses WebSocket/SSE, stale-reuse reconnect, sticky fallback, reasoning/usage, explicitly gated native compaction plus portable summary |
| `src/mimir/providers/openai_compatible_chat.rs` | OpenAI API and configurable OpenAI-compatible Chat Completions routes |
| `src/mimir/providers/anthropic_messages.rs` | Anthropic OAuth/API Messages, reasoning continuity, cache/clear controls, capability-gated compact probe |
| `src/mimir/providers/antigravity.rs` | Gemini Code Assist stream and tool-call thought-signature continuity |

Five provider ids are implemented: `openai-codex`, `openai`, `anthropic`,
`antigravity`, `openai-compatible`. Model defaults and env precedence live in
`selection.rs`; do not infer them from a dated benchmark model name.

## Tools and safety

| Module | Responsibility / seams |
| --- | --- |
| `src/tools/mod.rs`, `src/tools/registry.rs` | `ToolState`, registry construction, schemas, classification and Nexus adapters; manifest-driven spawn/status/cancel/output/plan/apply surface |
| `src/tools/path.rs` | Main-session path resolution and opt-in confinement; strict worker-path helpers |
| `src/tools/observe.rs`, `src/tools/text.rs` | Read-before-mutate observations, content freshness, bounded text and atomic writes |
| `src/tools/read.rs`, `src/tools/write.rs`, `src/tools/edit.rs` | Text reads/skim, atomic create/replace, exact-string/fallback edits and previews |
| `src/tools/grep.rs`, `src/tools/find.rs`, `src/tools/ls.rs` | In-process search/discovery/listing with bounds and omission accounting |
| `src/tools/bash/` | One-shot/persistent/background shells, process cleanup, capture, filtering and Landlock backend |
| `src/tools/bash/filter/` | Native/declarative output reducers and measured corpus; raw fallback on failure |
| `src/tools/ask_user_question.rs`, `src/tools/request_compaction.rs` | Required human interaction and opt-in one-shot compaction request flag |
| `src/tools/web/` | Opt-in approval-gated search/page readers, global egress policy, SSRF boundaries and bounded extraction |
| `src/tool_display.rs`, `src/tool_summary.rs`, `src/display_path.rs` | Presentation summaries, compact result classification and path display |

`read_output` and `recall` are registered read-only tools for output handles and
current-session transcript originals. File mutation requires freshness and the
applicable approval policy. Main-session confinement requires
`IRIS_SECURITY_OPT_IN=1`; Landlock permits workspace/temp writes and constrains
TCP only on supported Linux kernels. macOS shell execution is unconfined.
See the [operator safety contract](../../README.md#safety-and-permissions).

## Terminal surfaces

| Module | Responsibility / seams |
| --- | --- |
| `src/ui/mod.rs`, `src/ui/text.rs` | `Ui`, `UiEvent`, `UiBridge`, plain I/O and approvals; blocking plain approval input is a known cancellation limit |
| `src/ui/tui_loop.rs`, `src/ui/harness_actor.rs` | Input/render orchestration, typed harness commands/events, live settings/steering and parked approvals |
| `src/ui/tui.rs`, `src/ui/tui/screen.rs` | TUI composition and `Screen` state; transcript, composer, activity, goals and context |
| `src/ui/screen_mode.rs`, `src/ui/terminal_surface.rs`, `src/ui/tui/pager.rs` | Pager/inline policy, ANSI serialization/diff, alternate-screen frame ownership |
| `src/ui/slash.rs`, `src/ui/steering.rs` | Actual slash `COMMANDS` registry and steering/follow-up queues; `/undo` and `/clear` are not commands |
| `src/ui/modal.rs`, `src/ui/settings_menu.rs`, `src/ui/selector.rs`, `src/ui/picker.rs`, `src/ui/login.rs` | Docked controls, settings hatches, selection, model/effort/auth orchestration |
| `src/ui/delegation_dashboard.rs`, `src/ui/ask_user_question.rs` | Live worker/worktree review and required-question dialogs |
| `src/ui/tui/transcript.rs`, `src/ui/tui/tool_render.rs`, `src/ui/tui/streaming/` | Transcript/tool panels, folds, live reasoning, paced streaming and terminal receipts |
| `src/ui/tui/component.rs`, `src/ui/tui/overlay.rs` | Component/container composition and docked focus routing; general floating overlays remain deferred |
| `src/ui/markdown.rs`, `src/ui/highlight.rs`, `src/ui/hyperlink.rs` | Markdown, shipped syntax highlighting and sanitized OSC-8 hyperlinks; images remain unsupported |
| `src/ui/textengine.rs`, `src/ui/clipboard.rs` | Unicode/ANSI width and clipping, marker-safe text handling, clipboard/OSC-52 ladder |

The [slash reference](../../README.md#slash-command-reference) documents operator
commands; `src/ui/slash.rs::COMMANDS` is the registry. The
[TUI design language](../TUI_DESIGN_LANGUAGE.md) owns rendering rules.

## Tests and benchmarks

| Location | Existing coverage / role |
| --- | --- |
| `src/nexus_tests.rs` | Loop, approval/cancellation, steering, transcript validity, handles and Git-workflow acceptance |
| `src/wayland/background_compaction_tests.rs`, `src/wayland/compaction_property_tests.rs` | Job lifecycle, boundaries, pair safety, repeated summary/fold replay |
| `src/wayland/incremental_persistence_tests.rs`, `src/wayland/compaction_task_tests.rs` | Round-trip/crash-prefix persistence and open-task carry |
| `src/wayland/recall_tests.rs`, `src/wayland/microcompaction_tests.rs`, `src/wayland/fold_tests.rs` | Original retrieval, reducer guards and fold timing |
| `src/mimir/providers/openai_codex_responses_tests.rs` | Transport/reconnect, cancellation, payload/usage and native compaction contracts |
| `src/session.rs`, `src/tools/`, `src/ui/`, `src/wayland/git_safety/` | Module-local storage, safety, tool, UI/frame and mutation/recovery suites |
| `crates/iris-subagent-runtime/tests/` | Host-neutral scheduling, recovery, groups, worktrees, reviewed apply and standalone contracts |
| `src/compaction_bench.rs`, `src/compaction_live_bench.rs` | Deterministic compaction arms and historical double-gated live protocol |
| `src/live_harness/` | Campaigns, scenarios including migrated T-series, lane configuration, metrics and reporting |
| `src/harness.rs`, `iris-bench/src/` | Benchmark facade and separate benchmark CLI |
| `src/bench_tokens/`, `src/bench_tokens_per_task.rs` | Retained legacy suite; retirement awaits validation (#573) |

For checks use `bash scripts/gate.sh`. Documentation-only changes get whitespace
validation, not Rust execution. Code changes run formatting, Clippy, tests and
maintenance checks; run `cargo test --locked -p iris-subagent-runtime` for the
independent crate. Live/paid checks are opt-in; commands and evidence scope live
in [HARNESS.md](../benchmarks/HARNESS.md) and [BENCHMARK_PLAN.md](../BENCHMARK_PLAN.md).

## Remaining gaps

Ready summaries still wait until hard pressure/manual drain; the turn-edge hard
wait is blocking. Provider blocking reads and plain approval input have remaining
cancellation limits. Planner/ledger, parent mode profiles, branching, multimodal
reads, patch editing, structured CLI events, Windows, macOS confinement and
GitHub automation remain planned. Per-worker routing, native skills, terminal
doctor, task rollback, output dereference and managed reviewed apply already exist.

The headline campaign ran and found baseline cheaper overall; measurement is not
pending, but a universal savings claim remains unsupported. See the
[roadmap](../ROADMAP.md) for acceptance gates rather than old milestone numbers.
