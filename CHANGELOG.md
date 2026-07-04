# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- System-prompt fragments are now fully internal (ADR-0026): the prompt is
  assembled only from the fragments built into the binary plus
  `AGENTS.md`/`CLAUDE.md` project docs, cwd, and date. Iris no longer
  materializes defaults into `~/.iris/fragments` and no longer loads `.md`
  fragments from `~/.iris/fragments` or a repo's `<cwd>/.iris/fragments`,
  removing the `.iris/fragments` system-prompt-injection surface. The per-project
  fragment-trust gate, its first-run prompt, and the fragment meaning of
  `/trust` are gone with it. Migration: previously materialized
  `~/.iris/fragments/*.md` files are left in place but are inert (never read);
  delete them freely. Users who relied on custom fragments should move that
  steering into `AGENTS.md`.

### Added

- Repurposed the per-cwd trust store as a persistent project permission policy
  (ADR-0027, issue #209). `~/.iris/trust.json` (HOME-owned, canonical-directory
  keyed, `IRIS_TRUST_PATH` override) now stores per-project grants: per-tool
  approval defaults for `write`/`edit`, per-command `bash` allows (exact
  command or prefix), and a stored (not yet enforced) sandbox posture. A new
  `[p]` ("always for this project") approval option persists a grant, so
  granted tools/commands auto-approve across sessions in that directory;
  `/trust` becomes the project-permissions editor (toggle `write`/`edit`,
  revoke bash grants). `IRIS_TRUST_PATH` overrides must be absolute paths
  outside the project directory. Precedence is session > project > global
  default. Invariants: the store is never read from a repo-committed file (a
  clone cannot pre-approve its own tools); destructive commands (`rm`, `dd`, ...)
  always re-prompt and can never be granted; policy loosens only through
  deliberate user action. Legacy tri-state `"trusted"`/`"untrusted"` entries in
  `trust.json` are ignored (fail closed) and overwritten on the next grant.

### Fixed

- Made the prebuilt-binary release self-sufficient from a manually pushed tag.
  `release.yml` now builds the shell installer (a cargo-dist global artifact the
  build matrix never produced) and creates the GitHub release with all archives,
  checksums, and the installer via `gh release create`. `release-plz.toml` sets
  `publish = false` and `git_release_enable = false` so release-plz only opens
  the version/CHANGELOG PR and does not race to create the release or publish to
  crates.io. crates.io is now an explicit later opt-in documented in
  `docs/RELEASING.md`. Previously a release depended on the token-gated
  release-plz job creating the release, and a tag it pushed with the default
  `GITHUB_TOKEN` would not have triggered the binary build at all.

- Corrected install documentation to state that prebuilt binaries, `install.sh`,
  crates.io installs, and prebuilt self-update become usable only after the
  first public release/publish; the current pre-release install path is
  `cargo install --git ... --locked` or a source checkout.
- `install.sh` no longer corrupts the archive path during install. POSIX `sh`
  has no local scope, so `verify_checksum` assigned a bare `archive` that
  clobbered the caller's, and the extract step then received a doubled path so
  every prebuilt install failed. Found running the installer end-to-end for the
  first time (issue #252).
- Added the `[profile.dist]` build profile that cargo-dist requires. Without it
  `dist build` and the release workflow failed with "profile `dist` is not
  defined" (issue #252).
- Mutating built-in tools (`bash`, `edit`, `write`) now require approval by
  default, independent of the workspace path/sandbox opt-in. In print mode this
  means they are denied unless `--approve` is passed, so headless runs cannot
  execute them silently.

### Added

- Validated the prebuilt-binary release path without cutting a public release
  (issue #252, follows #199/#233): `scripts/validate-dist.sh` builds a real host
  archive + SHA-256 and exercises the real `install.sh` (download, checksum
  verify, atomic install) and `iris update` self-replace (download, verify,
  self-replace, already-latest, checksum-mismatch refusal) against a local
  server and a mock release response. Regression tests lock the asset/checksum
  names and the `DIST_VERSION`/`cargo-dist-version` sync; `docs/RELEASING.md` is
  the operator runbook for the remaining externally-visible steps (public
  release, crates.io token). `install.sh` gains an `IRIS_RELEASE_BASE_URL`
  override and `iris update` a loopback-only `IRIS_UPDATE_RELEASES_API_URL`
  override for local validation. Prebuilt/crates.io installs still become usable
  only after the operator cuts the first public release.
- Made model and reasoning switching token-efficient (ADR-0041): switches now
  classify as reasoning-only (silent; the request prefix is unchanged), model
  change, or provider change, and a model/provider switch carrying a large
  context appends an advisory with the estimated tokens the new model will
  re-read uncached and `/compact` as the way to shrink first. Foreign-origin
  reasoning rows are no longer replayed to any provider after a switch (the
  Anthropic lane previously downgraded them to text and the OpenAI-compatible
  lane replayed them as assistant content, re-billing the old model's
  chain-of-thought on every request); they stay persisted and display-visible.
- Added provider-backed compaction summaries and a manual `/compact` command.
  Compaction (auto and manual) now defaults to asking the active model for a
  structured handoff summary (goal, state, key facts, next steps) that reuses
  the cached context prefix; failures, empty answers, or non-shrinking
  summaries fall back to the deterministic bounded excerpts, and Ctrl-C skips
  compaction. `/compact` works in the TUI (turn-style spinner and cancel) and
  the text path, keeps a small recent tail, reports the token shrink, and
  needs no budget. New project-tunable setting: `compactionSummarizer`
  (`provider` default, `excerpts` for the deterministic stand-in).

- Added session shortcuts and pickers (issue #201): `iris -c`/`--continue`
  resumes the newest session for the current directory, `iris resume` opens the
  resume picker on a rich TTY or prints the resumable-session list in plain
  mode, and in-session `/resume` and `/new` swap the live session at a turn
  boundary without restarting the process.

- Added a headless `--print` mode (issue #200): `iris -p "prompt"` (or
  `iris --print "prompt"`) runs one agent turn-sequence, prints the final
  assistant answer to stdout, and exits 0 on success / nonzero on failure.
  Piped stdin is merged into the prompt after a blank-line delimiter
  (`cat log | iris -p "explain this failure"`); on a TTY there is nothing to
  merge. Print mode is non-interactive and never prompts: gated tools are denied
  by default, or auto-approved with `--approve`, so a pipe/CI run cannot hang.
  It persists its session like a normal run. (The project-trust default
  mentioned here was removed by ADR-0026; persisted project permission grants,
  ADR-0027, apply headless too.)

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
