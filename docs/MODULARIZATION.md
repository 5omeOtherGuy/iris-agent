# Iris — Modularization Plan: the Workspace Split

> Status (2026-07-16): proposed. This is the complete map for promoting Iris's
> in-crate tiers to a pi-mono-style cargo workspace in which every aspect of the
> project is an independently usable crate, as already done for
> `iris-subagent-runtime` (ADR-0063). When accepted it supersedes the
> "Packaging" section of [`ARCHITECTURE.md`](ARCHITECTURE.md) and revisits
> [ADR-0001](adr/0001-keep-nexus-wayland-iris-as-in-crate-tiers.md), whose own
> threshold — split when reuse justifies it — this plan claims is now met.
> Nothing in this document changes runtime behavior; every phase is a
> behavior-preserving refactor.

## What this is

A file-complete inventory of `src/` and `crates/`, the target crate graph, the
exact dependency edges that violate it today (with file:line evidence), the cut
for each violation, and a phased migration order with gates. The tier model and
dependency direction from [`ARCHITECTURE.md`](ARCHITECTURE.md) are unchanged;
this plan turns module discipline into compile-time boundaries.

## When to act

- Execute a phase only as an explicitly scoped task; do not fold phases into
  feature work.
- Every phase ends with `bash scripts/gate.sh` green and zero behavior change.
- Publishing any new crate to a registry is operator-only (release policy).

## Why now

ADR-0001 deferred the split until "a second front-end or published Nexus
runtime makes the split pay for itself." Current consumers that already pay:

- `iris-subagent-runtime` proved the pattern: host-neutral crate, no upward
  imports, own tests and examples (ADR-0063).
- `iris-bench` is a second front-end consuming the curated `iris_agent::harness`
  facade (ADR-0051).
- Headless `--print` mode and the live-harness/bench suites are additional
  non-TUI front-ends living inside the monolith.
- The compaction benches, provider adapters, and TUI each carry heavyweight
  dependency sets (syntect/ratatui vs reqwest/rustls vs landlock) that every
  consumer currently compiles together.

Goal, stated per the claims rule: independent use per aspect, compile-time
enforcement of the inward dependency direction, and smaller rebuild units.
No performance or adoption claims are made until measured.

## Reference model: pi-mono

```
pi-ai (leaf)          pi-tui (leaf)
   ▲                     ▲
   │                     │
pi-agent-core            │
   ▲                     │
   ╰────── pi-coding-agent ──▶ (both)
                 ▲
           pi-orchestrator
```

Iris differs in one deliberate way: the `ChatProvider` contract lives in the
core (Nexus), so the provider package (Mimir) depends on the core rather than
the reverse (see [`NAMING.md`](NAMING.md)). The independence property this plan
targets is the same one pi-mono has: the provider layer, the TUI library, the
core loop, the harness, and the orchestrator are each usable without the rest.

## Target workspace

```
                          ╭──────────────╮
                          │  iris-bench  │  bench control/analysis bin
                          ╰──────┬───────╯
                                 │ harness facade only
                          ╭──────▼───────╮
                          │  iris-agent  │  product: bin `iris`, CLI, app UI,
                          │  (root)      │  adapter tools, harness facade
                          ╰──┬──┬──┬──┬──╯
              ╭──────────────╯  │  │  ╰───────────────────╮
              ▼                 ▼  ▼                      ▼
      ╭──────────────╮ ╭────────────╮ ╭───────────╮ ╭──────────╮
      │ iris-wayland │ │ iris-mimir │ │iris-tools │ │ iris-tui │
      │ harness      │ │ providers  │ │ built-ins │ │ terminal │
      ╰──┬───┬───┬───╯ │ + auth     │ ╰──┬──┬──┬──╯ │ library  │
         │   │   │     ╰──┬─────┬───╯    │  │  │    ╰──┬───┬───╯
         │   │   ╰────────┼──╮  │        │  │  │       │   │
         │   ▼            ▼  │  │        │  │  │       │   │
         │ ╭───────────────╮ │  │        │  │  │       │   │
         │ │ iris-sessions │ │  │        │  │  │       │   │
         │ │ JSONL + goal  │ │  │        │  │  │       │   │
         │ │ + handles     │ │  │        │  │  │       │   │
         │ ╰──────┬────────╯ │  │        │  │  │       │   │
         │        ▼          ▼  ▼        ▼  │  │       │   │
         │      ╭──────────────────╮        │  │       │   │
         ╰─────▶│    iris-nexus    │◀───────╯  │       │   │
                │ core loop +      │           │       │   │
                │ contracts        │           │       │   │
                ╰────────┬─────────╯           │       │   │
                         │                     │       │   │
   ╭─────────────────────┼─────────────────────┼───────┼───┼──────╮
   │ foundation          ▼                     ▼       ▼   ▼      │
   │  ╭────────────────╮ ╭──────────────╮ ╭──────────────────╮    │
   │  │ iris-workspace │ │ iris-process │ │ iris-textengine  │    │
   │  │ path safety +  │ │ proc groups  │ │ width/ANSI/wrap  │    │
   │  │ atomic writes  │ │ + signals    │ ╰──────────────────╯    │
   │  ╰────────────────╯ ╰──────────────╯ ╭──────────────╮        │
   │                                      │ iris-config  │        │
   │                                      │ settings I/O │        │
   │                                      ╰──────────────╯        │
   ╰──────────────────────────────────────────────────────────────╯

   iris-subagent-runtime: unchanged leaf (no internal deps);
   consumed by iris-wayland and iris-agent.
```

### Crate roster

| Crate | Tier label | pi analogue | Contents (summary) | Internal deps | External deps (main) | Publish |
|---|---|---|---|---|---|---|
| `iris-textengine` | foundation | pi-tui text core | display width, grapheme segmentation, ANSI/OSC/APC strip, wrap/truncate/slice | none | unicode-width, unicode-segmentation | later, after API review |
| `iris-workspace` | foundation | — | workspace path confinement, lexical normalize, display paths, atomic writes, truncation helpers | none | anyhow, rand | later |
| `iris-process` | foundation | — | process-group spawn/kill/reap registry, SIGINT policy, force-quit restore flag | none | libc | later |
| `iris-config` | foundation | pi config.ts | settings discovery/merge/atomic save, raw serde model, common sections | iris-workspace | serde, serde_json, anyhow | later |
| `iris-nexus` | Nexus (core) | pi-agent-core | agent loop, `Message`/`ToolCall` contracts, `ChatProvider`, `Tool`/`Tools`, `ApprovalGate`, `AgentEvent`, `ContextGovernor`, `MutationGuard`, compaction/cache contracts, usage math, boundary errors, telemetry redaction | iris-workspace | tokio, tokio-util, futures, serde_json, thiserror, tracing | later |
| `iris-sessions` | Wayland-adjacent store | pi harness `session/` | JSONL `SessionLog`/`SessionStore`/span reader, oversized-output `HandleStore`, goal model + persistence | iris-nexus | serde_json, sha2, rand | later |
| `iris-mimir` | Mimir | pi-ai | provider adapters (Codex Responses, Anthropic Messages, Antigravity/Gemini, OpenAI-compatible chat), shared transport/retry, OAuth + token stores, selection, catalog, capabilities | iris-nexus, iris-config | reqwest, base64, sha2, rand, url | later |
| `iris-tools` | Iris tools | pi coding-agent tools | read/write/edit/bash (+sandbox/session/jobs/filters)/grep/find/ls/web/recall/read_output/ask_user_question/request_compaction, `ToolState`, observe, diff previews | iris-nexus, iris-workspace, iris-process, iris-config | grep, ignore, globset, landlock, similar, reqwest, regex, toml, dom_smoothie, htmd, dom_query, encoding_rs, url | later |
| `iris-wayland` | Wayland | pi harness | `Harness`, compaction engine/governor/background/trigger/fold, structured-summary extraction, skills, system prompt, trust, git safety, worker-runtime + subagent backend adapters | iris-nexus, iris-sessions, iris-config, iris-workspace, iris-subagent-runtime | serde_yaml_ng, toml, similar, sha2 | later |
| `iris-tui` | Iris UI library | pi-tui | terminal surface renderer, `Component`/`Container`, wrap, markdown renderer + theme, syntax highlight, selector, symbols, clipboard, alt-screen guard/pager surface, frame stats | iris-textengine, iris-process | ratatui, ratatui-textarea, pulldown-cmark, syntect, ansi-to-tui, base64 | later |
| `iris-agent` (root) | Iris product | pi-coding-agent | bin `iris`, CLI driver, app UI (Screen/tui_loop/modals/menus), approval UX, tool display/summary, print mode, self-update, git status, adapter tools (subagent/goal), harness facade, cross-tier tests + benches | all of the above | — | already published |
| `iris-bench` | tooling | — | benchmark control/analysis binary | iris-agent (facade only) | ratatui | no |
| `iris-subagent-runtime` | support | pi-orchestrator (loosely) | host-neutral worker scheduling, worktrees, groups, artifacts, apply plans | none | tokio, serde, sha2 | flip from `publish = false` when ready |

Naming follows [`NAMING.md`](NAMING.md): tier names (`nexus`, `wayland`,
`mimir`) label the tier crates; pure infrastructure takes descriptive non-myth
names, as Nexus itself precedents.

## The dependency law

Allowed edges only; everything else is a defect. Enforced by crate boundaries
after the split and by a check script during migration (see Verification).

```
iris-agent      -> every workspace crate
iris-bench      -> iris-agent (harness facade only)
iris-wayland    -> iris-nexus, iris-sessions, iris-config, iris-workspace,
                   iris-subagent-runtime
iris-tools      -> iris-nexus, iris-workspace, iris-process, iris-config
iris-mimir      -> iris-nexus, iris-config
iris-tui        -> iris-textengine, iris-process
iris-sessions   -> iris-nexus
iris-nexus      -> iris-workspace
iris-config     -> iris-workspace
foundation + iris-subagent-runtime -> (no internal deps)
```

Two independence properties fall out:

- `iris-mimir` and `iris-wayland` do not know each other. The harness reaches
  providers only through injected factories (`ChildProviderFactory` today);
  providers reach compaction only through Nexus contracts.
- `iris-tools` and `iris-wayland` do not know each other. The harness holds
  tool state opaquely and receives child tool sets through an injected factory.

## Complete module map

Every `.rs` file under `src/` and `crates/`, mapped to its target crate.
Corpus/fixture data files move with their owning module. This table is the
"everything" inventory; a phase is not done until its rows are relocated or
explicitly re-homed with a note here.

### Top-level `src/`

| File | Target crate | Notes |
|---|---|---|
| `src/main.rs` | iris-agent | bin shim |
| `src/lib.rs` | iris-agent | `run_cli`, dispatch; sheds module decls as crates split off |
| `src/cli.rs` | iris-agent | session driver, `ModelSwitch`, slash builders |
| `src/print.rs` | iris-agent | headless `--print` front-end |
| `src/approval.rs` | iris-agent | terminal decision parsing |
| `src/tool_display.rs` | iris-agent | presentation formatter for tool lines |
| `src/tool_summary.rs` | iris-agent | output-derived panel count strings |
| `src/selfupdate.rs` | iris-agent | product-specific updater; not a reusable aspect |
| `src/harness.rs` | iris-agent | curated bench facade (ADR-0051); staying in the product crate keeps `iris-bench` decoupled from internal crates |
| `src/errors.rs` | iris-nexus | Tier-1 boundary errors + exit codes (per ARCHITECTURE tier table) |
| `src/telemetry.rs` | iris-nexus | tracing init, secret redaction, external-body sanitization |
| `src/metrics.rs` | iris-nexus | pure usage math over `ProviderUsage`; see cut C7 |
| `src/display_path.rs` | iris-workspace | display-path helpers |
| `src/process_group.rs` | iris-process | process-group registry |
| `src/signals.rs` | iris-process | SIGINT policy + restore-arbitration flag |
| `src/handles.rs` | iris-sessions | `HandleStore` implements `nexus::ToolOutputStore` |
| `src/session.rs` | iris-sessions | JSONL log/store/span reader |
| `src/goal.rs` | iris-sessions | goal model; cohabits to kill the cycle (cut C8) |
| `src/goal_tests.rs` | iris-sessions | tests |
| `src/config.rs`, `src/config/tool_result_compaction.rs` | iris-config | after cut C6 inversion |
| `src/nexus.rs` | iris-nexus | after cuts C1; bench mod decls relocate (C12) |
| `src/nexus_tests.rs` | split | core-loop tests -> iris-nexus; cross-tier integration -> iris-agent `tests/` |
| `src/structured_summary_probe.rs` | iris-mimir | cfg(test) provider probe support |
| `src/bench_tokens_per_task.rs`, `src/bench_tokens/*` | iris-agent | cfg(test) cross-tier benches (workloads, analysis, fixtures, provider, observer, arms, probes, runner) |
| `src/compaction_bench.rs`, `src/compaction_live_bench.rs` | iris-agent | ADR-0045 production-seam benches; whole-stack by design |
| `src/live_harness/*` | iris-agent | live scenario runner (mod, runner, scenario, tool_scenarios, verdict, economics, probes, support, lanes, campaign) |
| `src/bench_fixtures/*` | iris-agent | bench fixture parsing |
| `src/git/mod.rs`, `src/git/status.rs` | iris-agent | session-bar git status (Tier-3 display concern) |

### `src/mimir/` -> `iris-mimir` (whole directory)

| File | Notes |
|---|---|
| `mod.rs`, `selection.rs`, `model_capabilities.rs`, `model_catalog.rs`, `anthropic_models.rs`, `retry.rs` | selection loses the `wayland::CacheProfile` import (C2); catalog `From` impl re-targets nexus-owned type (C3) |
| `auth/{mod,storage,device_code,oauth_callback,openai_codex,anthropic,antigravity,api_key}.rs` | unchanged content; standalone OAuth + token stores are a headline independent aspect (pi-ai analogue) |
| `providers/{mod,transport,openai_codex_responses,openai_codex_responses_tests,anthropic_messages,antigravity,openai_compatible_chat}.rs` | schema/virtual-tool references move to nexus contracts (C2); tests stop importing `crate::tools::built_in_tools` (C4) |

### `src/wayland/` -> `iris-wayland` (whole directory, two exceptions)

| File | Notes |
|---|---|
| `mod.rs` | `Harness`; `CacheProfile` type moves to iris-nexus (C2); `ToolState` becomes opaque host state (C10) |
| `compaction.rs`, `compaction_governor.rs`, `compaction_background.rs`, `trigger.rs`, `fold.rs` | fold's path helper comes from iris-workspace (C10) |
| `structured_summary/{mod,extraction,durable_text,input_renderer,validate}.rs` | stay; `structured_summary/schema.rs` moves to iris-nexus (C2) |
| `worker_runtime.rs`, `subagents.rs` | child tool sets via injected factory (C10) |
| `skills/{mod,model,loader,injection,render}.rs`, `skills_tests.rs` | loader path/read helpers from iris-workspace; `injection.rs`/`render.rs` are Codex-derived (Apache-2.0 SPDX) — NOTICE travels with the crate |
| `system_prompt/{mod,defaults,onboarding}.rs` + tests | uses `nexus::Tools` only — legal edge |
| `trust.rs` | per-project permission policy |
| `git_safety/{mod,checkpoint,settlement,net_diff,snapshot,task_state,baseline,ledger,git,jj,lock}.rs` | implements `nexus::MutationGuard`; path/text helpers from iris-workspace |
| `git_safety/{tests,checkpoint_tests}.rs`, `microcompaction_tests.rs`, `background_compaction_tests.rs`, `compaction_property_tests.rs`, `fold_tests.rs`, `incremental_persistence_tests.rs`, `compaction_task_tests.rs`, `recall_tests.rs` | drop `crate::{cli,ui}` imports (C9): local doubles or relocate to iris-agent `tests/` |

### `src/tools/` -> `iris-tools` (with extractions)

| File | Target | Notes |
|---|---|---|
| `path.rs`, `text.rs` | iris-workspace | shared by nexus, config, wayland, tools |
| `mod.rs` | iris-tools | `ToolState` stays tool-owned; injected opaquely (C1/C10) |
| `registry.rs` | split | workspace-tool registry stays; subagent adapter tools move to iris-agent (C5) |
| `read.rs`, `write.rs`, `edit.rs`, `observe.rs`, `ls.rs`, `grep.rs`, `find.rs` | iris-tools | corpus data modules (`grep_corpus/corpus.rs`, `ls_corpus/corpus.rs`) move with their tools |
| `skim.rs`, `skim/corpus.rs` | iris-tools | opt-in skim filter for `read` (ADR-0036) |
| `bash/{mod,sandbox,session,jobs}.rs`, `bash/filter/{engine,command,corpus}.rs`, `bash/filter/structured/{mod,git_diff,git_log,git_status,cargo_build,cargo_test,npm_test}.rs` + corpus data | iris-tools | landlock stays target-gated here |
| `web/{tool,policy,extract,excerpts,fetch,live_quality,corpus}.rs`, `web/search/{mod,filters,normalize,brave,duckduckgo,jina,searxng}.rs`, `web/read/{mod,native,jina}.rs`, `web_search.rs`, `read_web_page.rs` | iris-tools | SSRF policy gate (`web/policy.rs`) is a security boundary — negative tests move with it; backend config parsing moves in from config (C6) |
| `recall.rs`, `read_output.rs` | iris-tools | production code reaches stores through nexus contracts; handle imports are test-only |
| `ask_user_question.rs`, `request_compaction.rs` | iris-tools | required-interaction and compaction-request flags ride nexus contracts / `ToolState` |
| `goal.rs` | iris-agent | goal tools bind the sessions-owned goal model to the app wiring (C5) |
| `bench_support.rs` | iris-tools (dev) | ADR-0036/0037 bench helpers as dev-only module |

### `src/ui/` -> `iris-tui` (generic) vs `iris-agent` (app)

Generic-library candidates (no Iris/session/provider knowledge after
extraction; final per-file audit happens in Phase 6):

| File | Notes |
|---|---|
| `textengine.rs` | -> iris-textengine (Phase 2; iris-tui re-exports) |
| `terminal_surface.rs` | inline renderer (ADR-0006) |
| `tui/component.rs`, `tui/wrap.rs`, `tui/text.rs` | Component/Container + width helpers |
| `markdown.rs` | themed renderer; `MarkdownTheme`/`HighlightFn` seams already injected |
| `highlight.rs` | syntect scope -> palette mapping |
| `selector.rs`, `symbols.rs` | selector-list primitives, design-system symbols |
| `palette.rs`, `theme.rs` | canonical color roles + theme trait (ADR-0042); markdown/highlight consume them through injected seams |
| `zwj_probe.rs`, `terminal_env.rs` | ZWJ shaping probe and terminal-environment detection; generic capability probing |
| `clipboard.rs` | OSC 52 / platform-tool chain |
| `tui/pager.rs` (partial) | `AltScreen` guard + `PagerSurface` generic; `compose_frame` stays app-side |
| `tui/frame_stats.rs`, `tui/pane.rs`, `tui/panel.rs` | render diagnostics + pane/panel primitives |

App-side (stays in iris-agent):

| File | Notes |
|---|---|
| `mod.rs` | `Ui`, `UiEvent`, `UiBridge`, turn-error classification |
| `text.rs` | text fallback front-end |
| `tui.rs`, `tui/screen.rs`, `tui/transcript.rs`, `tui/rows.rs`, `tui/tool_render.rs`, `tui/shell_command.rs`, `tui/overlay.rs`, `tui/startup.rs`, `tui/activity.rs`, `tui/streaming/{mod,collector,chunking,controller,escapement,table_holdback}.rs` | Screen state, transcript rows, tool panels, SHELL command display, focus/overlay, startup, streaming pipeline |
| `tui_loop.rs`, `harness_actor.rs`, `steering.rs` | event loop, harness actor, steering queue |
| `slash.rs`, `modal.rs`, `picker.rs`, `login.rs`, `settings_menu.rs`, `session_menu/{mod,git_menu,tree_menu,jj_menu}.rs`, `delegation_dashboard.rs`, `ask_user_question.rs`, `screen_mode.rs`, `hyperlink.rs`, `terminal_doctor.rs`, `task_view.rs` | command surface, modals, menus, delegation UX, screen-mode policy, hyperlink emission, `/terminal-setup` doctor, task projection (imports session + git_safety — app-side by necessity) |

### `crates/` and `iris-bench/`

| Path | Target | Notes |
|---|---|---|
| `crates/iris-subagent-runtime/**` | unchanged | the exemplar; flip `publish = false` only with operator approval |
| `iris-bench/**` | unchanged | facade consumer; must compile untouched through every phase |

## Seam cuts (the actual work)

Each cut is small, testable, and lands before any crate move. Evidence is
current as of 2026-07-16.

| ID | Violation (evidence) | Cut |
|---|---|---|
| C1 | Nexus imports concrete tool state and helpers: `src/nexus.rs:1253` (`RefCell<crate::tools::ToolState>` in `ToolEnv`), `src/nexus.rs:1236` (`tools::path::workspace_relative`), `src/nexus.rs:2887` (`display_path`) | `ToolEnv` carries opaque host state (`&dyn Any` downcast owned by the tools crate, or a generic parameter — decide in Phase 1 spike); path/display helpers move to iris-workspace, which Nexus may depend on as foundation |
| C2 | Mimir imports Wayland types: `src/mimir/selection.rs:24` (`wayland::CacheProfile`, defined `src/wayland/mod.rs:245`); `src/mimir/providers/openai_codex_responses.rs:1614,1646` and `src/mimir/providers/anthropic_messages.rs:1700,1744` (`wayland::structured_summary::{canonical_compaction_schema, VIRTUAL_TOOL_NAME}`) | move `CacheProfile` and `structured_summary/schema.rs` into Nexus contracts (Nexus already owns opaque compaction values, usage metadata, and the capability enum per ARCHITECTURE); Wayland and Mimir both import them from Nexus |
| C3 | Mimir -> metrics: `src/mimir/model_catalog.rs:297` (`From<EffectiveContextWindow> for metrics::ContextWindowFacts`) | metrics.rs moves into iris-nexus (pure math over `ProviderUsage`), making the edge mimir -> nexus |
| C4 | Mimir tests -> tools/app: `src/mimir/providers/openai_compatible_chat.rs:803` and `anthropic_messages.rs:2770+` (`crate::tools::built_in_tools()`), provider probe fns referencing cfg(test) `structured_summary_probe` | provider tests build local fixture tools; probe module moves into iris-mimir test support |
| C5 | Tools -> wayland/mimir/goal/print: `src/tools/registry.rs:65-66` (`SubagentBackend`, `ChildProviderFactory`), `:399-413` (`mimir::selection` routing, `ChildRoute`), `src/tools/goal.rs:89` (`crate::goal`), `src/tools/registry.rs:2302` (test-only `print::UsageBase`) | subagent adapter tools and goal tools move to an iris-agent adapter module (they bind harness + provider + goal state); the registry keeps only workspace tools plus an extension point for host-supplied tools; the usage-estimate test moves with the adapter module |
| C6 | Config -> tools/wayland: `src/config.rs:589` (`tools::path::lexical_normalize`), `:733-747` (`tools::web::{SearchBackend,ReadBackend}`), `:815-842` (`wayland::{SummarizerKind, CompactionWorkerConfig, ...}`) | config keeps discovery/merge/save + raw serde data; interpretation inverts to the owning crate (`SummarizerKind::from_settings(&Settings)` in wayland, backend parsing in tools, normalize from iris-workspace) |
| C7 | Metrics -> config: `src/metrics.rs:210-212` (`DEFAULT_COMPACTION_{WARN,START,HARD}`) | thresholds are passed in at construction (or the defaults move beside the trigger contract in Nexus); metrics keeps zero config knowledge |
| C8 | Session <-> goal cycle: `src/session.rs:55` (`use crate::goal::Goal`), `src/goal.rs:81` (`session::new_session_id`) | cohabit in iris-sessions (goal is one durability domain with the transcript); a later record-shape split stays possible |
| C9 | Wayland tests -> cli/ui: `src/wayland/git_safety/tests.rs:1533+` (`cli::run_print_turn`), `src/wayland/microcompaction_tests.rs:1023` (`ui::UiEvent`), `src/wayland/background_compaction_tests.rs:27` (`ui::steering::SteeringQueue`) | replace with harness-local doubles where the assertion is harness behavior; move genuinely cross-tier scenarios to iris-agent `tests/` |
| C10 | Wayland -> tools (production): `src/wayland/mod.rs` (`ToolState` instance), `src/wayland/subagents.rs` (child tool sets), `src/wayland/fold.rs:270`, `git_safety/{snapshot,task_state}.rs`, `skills/loader.rs`, `structured_summary/extraction.rs` (path/text helpers) | harness holds opaque host state (same seam as C1) and receives a `ToolSetFactory` injected by the app, mirroring `ChildProviderFactory` (ADR-0063); path/text helpers come from iris-workspace |
| C11 | UI generic/app entanglement: generic candidates import app modules (e.g. `tui/overlay.rs` -> `ui::slash`, `markdown`/`highlight` -> palette) | Phase 6 audit: generic pieces take injected themes/data (the `MarkdownTheme`/`HighlightFn` seams already exist); overlay's palette view stays app-side if the data contract does not generalize cleanly |
| C12 | Bench modules declared inside the core: `src/nexus.rs:4127-4152` (`mod bench_tokens_per_task/compaction_bench/compaction_live_bench/live_harness`) | relocate declarations to `lib.rs` cfg(test) (Phase 1) and to iris-agent `tests/` at the split |

## What stays together on purpose

- `selfupdate`, `print`, `git/status`, approval UX, tool display: product
  concerns, not reusable aspects. They stay in iris-agent.
- Goal + sessions: one durability domain (C8). No `iris-goal` crate.
- No `iris-testkit` crate until at least two crates duplicate fixtures.
- No plugin runtime, no new tiers, no renames (Heimdall stays deferred), no
  behavior changes, no API redesign beyond what a cut requires.
- `iris-bench` and the harness facade keep their current shape (ADR-0051).

## Migration sequence

Ordering rule: cuts first (in-crate, reviewable, gate-green), then mechanical
crate moves from the leaves up. The root crate temporarily re-exports moved
paths (`pub use iris_nexus as nexus;`) so each phase stays small; shims are
removed in the final phase.

| Phase | Scope | Gate (all phases also: `bash scripts/gate.sh` green, behavior-identical) |
|---|---|---|
| 0 | Land this plan; add `scripts/check-layering.sh` asserting the module-level edge list from "The dependency law" via import scan | check script green on main after C-cuts land, red on injected violation |
| 1 | Cuts C1-C12 as separate commits/PRs inside the single crate | forbidden-import scan reports zero production violations; `nexus.rs` imports no `crate::tools` concrete state |
| 2 | Extract foundations: `iris-textengine`, `iris-workspace`, `iris-process` (new workspace members under `crates/`) | each crate builds + tests standalone (`cargo test -p <crate>`); root re-export shims in place |
| 3 | Extract `iris-nexus`, then `iris-sessions` | `cargo check -p iris-nexus` pulls no reqwest/ratatui/landlock; nexus_tests split lands |
| 4 | Extract `iris-config`, `iris-mimir`, `iris-tools` | mimir builds without wayland/tools in its tree; a `mimir` example authenticates + streams standalone (mirrors subagent-runtime `examples/`) |
| 5 | Extract `iris-wayland` (with `iris-subagent-runtime` dep) | headless harness example runs a scripted turn with a fake provider and injected tool factory |
| 6 | Extract `iris-tui` after the C11 per-file audit | a non-Iris demo binary renders with `iris-tui` only; TUI golden/frame tests unaffected |
| 7 | Finalize: remove re-export shims, per-crate READMEs + doc headers, workspace `[workspace.package]` inheritance (edition/rust-version/license), release wiring, docs updates | `scripts/validate-dist.sh` passes; CODEMAPS/ARCHITECTURE/AGENTS project map updated; new ADR recorded as accepted |

Worktree discipline applies: each phase is its own task worktree
(`bash scripts/worktree-create.sh ../iris-<slug> <branch>`).

## Verification

- `bash scripts/gate.sh` after every commit of every phase (format, Clippy,
  tests, maintenance checks).
- `scripts/check-layering.sh` (Phase 0): during migration it scans
  `use crate::` per module against the edge list; after each extraction the
  compiler enforces the moved boundary and the script shrinks.
- Standalone builds: `cargo check -p <crate>` and `cargo test -p <crate>` for
  every member; dependency-set assertions via `cargo tree -p <crate>` (e.g.
  iris-nexus must not pull reqwest, ratatui, or landlock).
- Independence proof per major crate: one runnable example under
  `crates/<name>/examples/`, the same standard `iris-subagent-runtime` set.
- Facade stability: `iris-bench` compiles and its analysis tests pass untouched
  in every phase.
- Behavior freeze: no new features, no schema changes to sessions/settings/auth
  files, byte-identical system prompt assembly (existing tests cover it).

## Release and publishing impact

- All new crates start `publish = false` (subagent-runtime precedent).
  Graduating any crate to crates.io is an operator-only decision per crate,
  after an API review; names should be reserved before announcement.
- `release-plz.toml` and cargo-dist currently assume a single published
  package. Phase 7 updates them: the product binary remains the only dist
  artifact; release-plz gains per-crate version/changelog config for members
  that graduate.
- Version policy: `[workspace.package]` inheritance for edition, rust-version
  (subagent-runtime pins 1.96), license; per-crate semver once published.
  Never republish or force-tag; fix forward (release policy).
- Licensing: Codex-derived files carry Apache-2.0 SPDX headers while the repo
  is MIT (e.g. `wayland/skills/injection.rs`). Each crate containing derived
  files ships the `NOTICE` context; audit per crate before any publish.

## Risks

- Public-surface inflation: `pub(crate)` becomes `pub` at each boundary.
  Mitigate: minimal `pub` + `#[doc(hidden)]` for seams that exist only for the
  product crate; API review before any publish.
- Orphan-rule breakage for cross-crate `From`/trait impls (C3 is the known
  case; others may surface at Phase 3-4). Mitigate: re-home impls beside the
  owned type before moving files.
- Test entanglement is the largest hidden cost: `nexus_tests.rs` (9k lines) and
  the wayland compaction suites exercise cross-tier seams. Budget explicit time
  for the C9/C12 splits; do not weaken assertions to make them move.
- Release-pipeline drift (release-plz + dist + install.sh assumptions).
  Mitigate: Phase 7 dry-runs `scripts/validate-dist.sh` and a release-plz
  `--dry-run` before any tag.
- Longer workspace-wide compile for contributors who previously touched one
  module; offset goal (unmeasured until benchmarked): smaller `-p` rebuild
  units for focused work.
- Churn conflicts with in-flight feature branches. Mitigate: phase PRs are
  move-only or cut-only, never mixed with behavior changes; announce before
  each extraction.

## Open questions

- Opaque host state mechanism for C1/C10: `&dyn Any` downcast vs generic
  `ToolEnv<S>`; decide with a Phase 1 spike (generic infects `Agent`'s
  signature; `Any` costs a runtime downcast per tool call).
- Root layout at Phase 7: keep root package `iris-agent` (fewest release-pipeline
  changes, recommended) vs virtual workspace root with the product under
  `crates/iris-agent`.
- Whether `telemetry` splits (redaction -> iris-nexus, `init()` -> iris-agent)
  or moves whole; depends on tracing-subscriber weight tolerance in iris-nexus.
- Whether `hyperlink.rs` and the reserved `textengine::ansi_aware` subsystem
  graduate into iris-tui now or after the deferred hyperlink UI feature ships.
- Crate name check: `iris-workspace` collides conceptually with cargo
  workspaces; alternative `iris-pathsafe`. Decide before Phase 2.

## References

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — tier ownership this plan compiles into
  crates.
- [`NAMING.md`](NAMING.md) — naming convention for the crate roster.
- [ADR-0001](adr/0001-keep-nexus-wayland-iris-as-in-crate-tiers.md) — the
  in-crate decision this plan supersedes on acceptance.
- [ADR-0051](adr/0051-iris-bench-workspace-split-and-harness-facade.md) — the
  facade contract `iris-bench` keeps.
- [ADR-0063](adr/0063-extract-subagent-runtime-and-centralize-worker-scheduling.md)
  — the extraction exemplar (host-neutral crate, injected factories).
- [`CODEMAPS/INDEX.md`](CODEMAPS/INDEX.md) — implemented-source map the
  inventory above was drawn from.
