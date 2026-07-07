# Iris OpenWiki

Offline, repository-local documentation for Iris. These pages describe the code
that exists today. Future or target-only items belong in `docs/ROADMAP.md`,
ADRs, or source comments, not as implemented behavior here.

## Start here

- [Architecture](architecture.md) explains Nexus, Wayland, Iris, and Mimir.
- [CLI Usage](cli-usage.md) covers launch modes, print mode, resume, login,
  update, display flags, danger mode, and slash commands.
- [Provider Authentication](provider-auth.md) covers OpenAI Codex, OpenAI API,
  OpenAI-compatible endpoints, Anthropic, Antigravity, credentials, settings,
  and environment variables.
- [Tools](tools.md) covers built-in tool behavior and safety boundaries.
- [Sessions and Storage](sessions-storage.md) covers transcripts, resume,
  compaction, output handles, permissions, and task checkpoints.
- [TUI Behavior](tui-behavior.md) covers terminal rendering and fallback modes.
- [Release and Update](release-update.md) covers installation, `iris update`,
  and release boundaries.
- [Contributor Workflow](contributor-workflow.md) covers worktrees, gates, and
  docs workflow.

## Source of truth

Trust code over prose when they conflict. Use:

- `docs/CODEMAPS/INDEX.md` for the current implementation map.
- `docs/ARCHITECTURE.md` for tier ownership rules.
- `docs/NAMING.md` for component names.
- `README.md` for user-facing command and settings examples.
- `docs/ROADMAP.md` for build order and deferred capabilities.

## Website

The separate `iris-wiki-site` repository imports `openwiki/` and builds the
static website. The website owns presentation. This repository owns the content.
