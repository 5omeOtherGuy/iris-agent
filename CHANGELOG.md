# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Added root-level product and design-system briefs (`PRODUCT.md` and
  `DESIGN.md`) and linked them from the README documentation index.
- Documented the PR #170-#175 TUI/harness batch: compact tool durations,
  `ctrl+o` full-output reveal, word-level diff highlights, pi-mono-aligned
  harness limits, the reusable TUI component/focus layer, the shared text
  engine, the tool renderer registry, richer assistant Markdown, and collapsed
  reasoning/thinking panels.
- Documented current post-0.1.0 capabilities: terminal-surface TUI ownership,
  provider/model/reasoning selectors, Mimir auth hardening, Anthropic browser
  OAuth, Antigravity `thoughtSignature` continuity, structured runtime events,
  provider usage/cache accounting, prompt-cache controls, and Anthropic
  context-management opt-ins.
- Added docs for the opt-in `scripts/tui-live.sh` manual live-rendering harness
  used when changing pane rendering.
- Added ADR-0022 for default-short provider-native prompt-cache and
  default-off context-management integration.

### Changed

- Design-system upgrade across the TUI pane: gated tool calls now render a
  `▲ REVIEW` review line (review glyph + label, a `$ ` prompt for shell
  actions) instead of a bare `approve …`; reasoning ("thinking") renders as a
  chromeless muted `┊ THINKING` left rail rather than a bordered panel, keeping
  its `ctrl+o` fold; and `EDIT` diff previews gain a quiet `+added −removed`
  footer tinted to the diff inks (with a `┊ new file` note for new files).
- Centralized the state/marker symbol vocabulary in `src/ui/symbols.rs` and the
  terminal-relative color roles in `src/ui/palette.rs` (adding the `▲` review
  glyph and a named `interactive`/Cyan role) as the single source of truth,
  replacing scattered glyph and `Color::Cyan` literals.
- Refreshed README, roadmap, and feature inventory against merged PR and git
  history through PR #177.
- Clarified current user-visible TUI behavior: state-specific panel symbols,
  preview/full output folding, GFM table/task-list/strikethrough rendering,
  collapsed thinking blocks, Unicode-aware wrapping, and generic safe fallback
  rendering for unknown tools.
- Clarified current harness/tool limits: no default bash timeout, no fixed
  default tool-roundtrip cap, full safe-parallel batches for read-only search
  tools, 50 KiB inline display threshold, and retained memory-safety rails.

## [0.1.0](https://github.com/5omeOtherGuy/iris-agent/releases/tag/v0.1.0) - 2026-06-17

### Added

- *(grep)* add output modes with structured metadata telemetry
- *(wayland)* move persistence + execution surface to a Tier-2 harness (Step C)
- *(nexus)* inject tools via Tool trait, resolve by name (Step B)
- *(provider)* richer Codex system prompt with explicit tool list, no-other-tools guard, and workspace cwd
- *(tools)* native grep + find via ripgrep library, drop rg/fd binaries ([#19](https://github.com/5omeOtherGuy/iris-agent/pull/19))
- *(session)* JSONL transcript persistence
- *(config)* provider/model settings file
- *(ui)* startup logo banner + README logo
- *(ui)* readable terminal UX for complex multi-tool work
- *(bash)* centralized process-group reaping on force-quit ([#3](https://github.com/5omeOtherGuy/iris-agent/pull/3))
- *(bash)* background jobs ([#3](https://github.com/5omeOtherGuy/iris-agent/pull/3))
- *(bash)* persistent shell sessions ([#3](https://github.com/5omeOtherGuy/iris-agent/pull/3))
- *(bash)* kernel sandbox via Landlock LSM ([#3](https://github.com/5omeOtherGuy/iris-agent/pull/3))
- *(tools)* structured ToolOutput result/metadata contract ([#15](https://github.com/5omeOtherGuy/iris-agent/pull/15)) + ls long mode ([#8](https://github.com/5omeOtherGuy/iris-agent/pull/8))
- *(tools)* read-before-mutate stale-file guard (#5/#11/#12)
- *(edit)* add replaceAll and actionable failure messages ([#4](https://github.com/5omeOtherGuy/iris-agent/pull/4))
- *(ls)* directories-first sort + recursive tree view ([#8](https://github.com/5omeOtherGuy/iris-agent/pull/8))
- *(tools)* actionable error when rg/fd is missing
- *(find)* native file search via ignore + globset ([#7](https://github.com/5omeOtherGuy/iris-agent/pull/7))
- *(nexus)* graceful SIGINT handler closes MVP exit gate ([#9](https://github.com/5omeOtherGuy/iris-agent/pull/9))
- *(cli)* add render seam and native TUI
- *(display)* add shared tool-call display formatter
- *(observability)* structured logging, retries, and typed errors ([#17](https://github.com/5omeOtherGuy/iris-agent/pull/17))
- *(tools)* write files atomically via temp-file and rename
- *(approval)* gate mutating tools behind user confirmation
- *(nexus)* display tool results in the REPL
- *(tools)* port pi native tools with process-group timeout kill ([#1](https://github.com/5omeOtherGuy/iris-agent/pull/1))
- *(nexus)* add read tool loop
- *(auth)* add browser OAuth login
- *(auth)* add OpenAI Codex device login

### Fixed

- *(maintenance)* address M1 review cleanup
- *(ui)* show real bash command in approval; re-prompt destructive always-allowed calls
- *(bash)* tidy session routing and faithful job output ([#3](https://github.com/5omeOtherGuy/iris-agent/pull/3))
- *(bash)* bound one-shot capture memory and harden Landlock fd ([#3](https://github.com/5omeOtherGuy/iris-agent/pull/3))
- *(bash)* address multi-reviewer findings on bash hardening ([#3](https://github.com/5omeOtherGuy/iris-agent/pull/3))
- *(edit)* align error messages with old_string/new_string param names
- *(tools,nexus)* address Milestone 1 review findings
- *(find)* restore fd parity in native glob matching
- *(tui)* clean up tool transcript display
- *(provider)* add jitter when honoring Retry-After backoff
- *(bash)* run commands with bash instead of sh ([#16](https://github.com/5omeOtherGuy/iris-agent/pull/16))
- *(nexus)* end tool loop gracefully at a raised round-trip cap
- *(read)* reject non-text files
- *(grep)* honor hashline option
- *(tools)* drop false image-attachment claim from read description

### Other

- *(deps)* bump grep from 0.3.2 to 0.4.1 ([#29](https://github.com/5omeOtherGuy/iris-agent/pull/29))
- *(deps)* bump sha2 from 0.10.9 to 0.11.0 ([#28](https://github.com/5omeOtherGuy/iris-agent/pull/28))
- *(deps)* bump amannn/action-semantic-pull-request from 5 to 6 ([#26](https://github.com/5omeOtherGuy/iris-agent/pull/26))
- *(deps)* bump actions/labeler from 5 to 6 ([#25](https://github.com/5omeOtherGuy/iris-agent/pull/25))
- *(deps)* bump dependabot/fetch-metadata from 2 to 3 ([#27](https://github.com/5omeOtherGuy/iris-agent/pull/27))
- add committed slim AGENTS.md with Codex review guidelines
- *(pr)* remind to mention @codex review to trigger Codex review
- auto-approve Dependabot patch/minor PRs so auto-merge can land
- pin release-plz action to v0.5
- tune typos allow-list and accept deps PR-title type
- add GitHub Actions, repo automation, and contributor docs
- *(runtime)* clarify async agent loop direction
- *(nexus)* invert front-end dependency with AgentObserver + ApprovalGate
- *(roadmap)* mark grep/find native (shipped via PR #19), close rg/fd packaging gap
- reframe WASM/plugins as exploratory, not a committed direction
- *(architecture)* document three-tier split and align tool/plugin work
- document planned WASM plugin integration (issue #18)
- *(roadmap)* keep preview as single diff surface; cut M1 self-review
- *(roadmap)* mark bash hardening ([#3](https://github.com/5omeOtherGuy/iris-agent/pull/3)) shipped across four subsystems
- *(providers)* extract openai_codex_responses tests to a separate file
- *(nexus)* extract tests to nexus_tests.rs; feat(ui): reprompt on invalid approval input
- *(tools)* single Claude-compatible edit path; remove hashline
- *(find)* wrap fd instead of reimplementing natively (Option A)
- correct single-static-binary overclaim in find rationale
- *(roadmap)* record real-provider smoke test; MVP gates met
- *(roadmap)* mark streaming and diff previews as shipped
- refresh codemap and README for Ui seam, streaming, and diff previews
- remove TUI, transcript, and YAGNI seams
- update MVP status and issue tracking
- *(tools)* split tools.rs into per-tool module tree
- *(roadmap)* add provider-specific tools workstream ([#10](https://github.com/5omeOtherGuy/iris-agent/pull/10))
- update current implementation status
- *(nexus)* simplify read file helper name
- *(nexus)* cover read tool edge cases
- ignore local agent instructions
- add solo git workflow
- Update project documentation
- Initial Iris agent prototype
