# Iris Agent Guide

This file is the public repository instruction source for coding agents. It is
project guidance, not a security boundary.

## Commands

- Create an isolated task worktree: `bash scripts/worktree-create.sh ../iris-<slug> <branch>`
- Check primary freshness only: `bash scripts/worktree-preflight.sh`
- Run a focused test: `cargo test --locked <test-name>`
- Run the CI-equivalent gate: `bash scripts/gate.sh`
- Validate distribution artifacts: `bash scripts/validate-dist.sh`
- Clean up after an operator-approved merge, from outside the task worktree:
  `bash scripts/worktree-cleanup.sh ../iris-<slug>`
- Reconcile a drifted primary checkout: `bash scripts/sync-primary.sh`
- Install repository hooks once per clone: `bash scripts/install-hooks.sh`

## Project map

- `src/nexus.rs`: provider-neutral agent loop and contracts.
- `src/wayland/`: harness state, sessions, compaction, skills, project
  instructions, permissions, and worker integration.
- `src/mimir/`: provider adapters, authentication, model selection, and
  capabilities.
- `src/ui/` and `src/cli.rs`: terminal UI and command/session drivers.
- `src/tools/`: built-in tool implementations and workspace safety.
- `crates/iris-subagent-runtime/`: host-neutral worker scheduler and worktree
  infrastructure.
- `docs/ARCHITECTURE.md`: tier ownership and runtime mechanics.
- `docs/CODEMAPS/INDEX.md`: implemented source map.
- `docs/ROADMAP.md`: build sequence; `docs/FEATURES.md` is an inventory.

## Architecture boundaries

- Keep dependencies inward: Iris CLI/adapters depend on Wayland, which depends
  on Nexus. Nexus imports no terminal UI, concrete tools, provider adapters,
  session storage, or approval UX.
- Wayland owns project-instruction assembly, sessions, compaction policy, skill
  loading, workspace execution state, and worker integration.
- Mimir owns provider names, endpoints, transport details, credentials, and
  provider-specific request/response behavior.
- The CLI owns terminal rendering and interaction. Do not move UI policy into
  Nexus or enforcement into the CLI.
- Follow the Tokio async model in `docs/ARCHITECTURE.md`. Do not add a custom
  runtime, bespoke session transport, or a monolithic agent module.
- Define contract seams before wiring Nexus, providers, tools, approvals, or UI.

## Security boundaries

- Treat workspace traversal, shell-policy bypass, and approval-gate bypass as
  blocking defects.
- Validate user, model, provider, tool, path, and shell input at system
  boundaries. Keep internal values lean when framework guarantees apply.
- Preserve workspace confinement, read-before-mutate checks, atomic writes,
  approval floors, cancellation, and transcript validity.
- Project instruction files are trusted steering, not enforcement. Project
  discovery must refuse symlinks and non-regular files and retain bounded reads
  and user-visible diagnostics.
- Never place credentials or private machine paths in tracked instructions,
  skills, fixtures, logs, or examples.
- Do not weaken checks, hide failures, hard-code test outcomes, or bypass hooks.

## Testing and completion

- Write a failing behavior test before production code for runtime changes.
- Add deterministic coverage for normal, boundary, and failure paths. Security
  changes require explicit negative tests.
- Prefer focused tests while iterating; run `bash scripts/gate.sh` before
  completion. The gate runs format, Clippy, tests, and maintenance-script checks.
- Every changed code path must run at least once. Report commands and exact
  results; do not claim success for skipped or failing checks.
- Keep live-provider and paid benchmark tests opt-in. Never spend provider
  credits unless the task explicitly requires it.

## Code standards

- Prefer the standard library, platform features, and existing dependencies
  before hand-rolled logic or new crates.
- Make the smallest correct change. Avoid speculative abstractions, broad
  refactors, and helpers used once.
- Use `Result` and contextual errors in production paths; avoid `unwrap()` and
  `expect()` outside tests or proved invariants.
- Keep modules cohesive and explicit. Do not split solely for line count.
- Preserve provider-neutral types in Nexus and exhaustive handling for policy
  enums.
- No emoji in code, comments, docs, logs, or CLI output.
- Use conventional commit subjects and keep commits focused.

## Documentation and skills

- Follow the `write-documentation` skill for repository docs: terse field-manual
  voice, progressive disclosure, measured claims, and placeholder examples.
- Use `documentation-codemap-specialist` when source ownership or module maps
  change. Update `docs/CODEMAPS/INDEX.md` from implemented code.
- Use `iris-tui` for work under `src/ui` or terminal rendering behavior.
- Canonical repository skills live under `.agents/skills/<name>/SKILL.md`.
  Claude projections under `.claude/skills/` are relative symlinks to that
  canonical source. Do not add duplicate `.pi/skills` copies.
- Keep skill directory names and frontmatter `name` fields identical. Run
  `bash scripts/check-repo-guidance.sh` after changing instructions or skills.

## Git and worktrees

- Work only in a task-specific worktree. The primary `main` checkout is
  control-only and must match `origin/main`.
- Use `scripts/worktree-create.sh`; plain `git worktree add` receives tracked
  files but does not copy ignored local instructions.
- Preserve unfamiliar changes and leave worktrees and branches you do not own
  untouched. Never reset, clean, or force-delete someone else's work.
- Tracked `AGENTS.md` is the public base. Tracked `CLAUDE.md` imports it.
- Ignored local layers have harness-specific semantics:
  - `AGENTS.override.md` replaces the same-directory base in Codex and Iris.
  - `AGENTS.local.md` is Iris's additive local layer.
  - `CLAUDE.local.md` is additive in Claude Code and Iris's local fallback.
  - `.pi/APPEND_SYSTEM.md` is the additive local prompt for trusted Pi projects.
- Iris processes directories root-to-leaf. In each directory it selects the
  first non-empty regular base from `AGENTS.override.md`, `AGENTS.md`, then
  `CLAUDE.md`, followed by the first non-empty local file from
  `AGENTS.local.md`, then `CLAUDE.local.md`.
- Commits, pushes, pull requests, reviews, merges, issue updates, and other
  shared remote changes require explicit operator direction.

## Release policy

- Releases are operator-only. Never push a version tag, publish a crate, create
  or publish a GitHub release, add registry credentials, or merge a release PR
  without explicit approval in the current turn.
- Prepare and verify with `docs/RELEASING.md` and
  `bash scripts/validate-dist.sh`; the operator performs public actions.
- Never rewrite or force-push `main` or a release tag. Fix forward.

## Optional references

- Decisions: `docs/adr/README.md`
- Naming and IDs: `docs/NAMING.md`
- Release runbook: `docs/RELEASING.md`
- TUI live checks: `docs/TUI_LIVE_TESTING.md` (only after pane-rendering changes)
- AGENTS.md format: <https://agents.md/>
