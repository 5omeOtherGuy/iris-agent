# Iris OpenWiki owner’s manual

`openwiki/` is Iris's repository-local, offline operations manual. It explains
implemented behavior by subsystem for operators and coding agents that need more
detail than the root `README.md`.

The pages are ordinary Markdown and require no website tooling to read. Start
with the root owner's manual for the product-wide contract, then use this index
to narrow the investigation.

## Choose a guide

| Need | Guide | Scope |
| --- | --- | --- |
| Understand ownership and dependency direction | [Architecture](architecture.md) | Nexus, Wayland, Iris, Mimir, async boundaries, and module placement. |
| Launch or control Iris | [CLI usage](cli-usage.md) | Interactive, print, resume, login, update, display flags, danger mode, and slash commands. |
| Configure a provider or credential | [Provider authentication](provider-auth.md) | OpenAI Codex, OpenAI API, OpenAI-compatible, Anthropic, Antigravity, stores, settings, and environment variables. |
| Use or maintain tools | [Tools](tools.md) | Built-in contracts, approval behavior, path/file safety, shell execution, and output bounds. |
| Diagnose persistence or context | [Sessions and storage](sessions-storage.md) | JSONL transcripts, resume, compaction, output handles, permissions, and task checkpoints. |
| Understand terminal behavior | [TUI behavior](tui-behavior.md) | Pager, inline and plain fallbacks, interaction, and terminal cleanup. |
| Install, update, or release | [Release and update](release-update.md) | Installer, `iris update`, artifacts, checksums, and operator-only release boundaries. |
| Change the repository | [Contributor workflow](contributor-workflow.md) | Required task worktrees, preflight, gate, merge, cleanup, and documentation workflow. |

## Status and evidence

OpenWiki describes shipped behavior. Planned or research-only designs belong in
`docs/ROADMAP.md` or an ADR and must be labeled there; do not copy them here as
working commands.

When sources conflict, use this order:

1. current code and tests;
2. `docs/CODEMAPS/INDEX.md` for the implementation map;
3. `docs/ARCHITECTURE.md` and `docs/NAMING.md` for ownership and names;
4. the root `README.md` for the current operator contract;
5. `docs/FEATURES.md` for the status-tagged inventory;
6. `docs/ROADMAP.md` for sequencing and deferred capabilities.

An accepted ADR records a decision; it does not by itself prove that all of the
design is implemented. Check code, tests, the codemap, and later amendments.

## Agent maintenance rules

- Keep one subsystem per page; link to the root manual instead of duplicating its
  full command or settings tables.
- State opt-in flags, platform limits, and unsafe fallbacks beside the capability.
- Use exact command, setting, event, type, and file names from current code.
- Distinguish defaults from examples and implemented behavior from roadmap work.
- Update this index whenever a page is added, renamed, or removed.
- Run the repository documentation/link checks through `bash scripts/gate.sh`
  before presenting an update as complete.

## Website boundary

The separate `iris-wiki-site` repository imports `openwiki/` and builds the
public static site. This repository owns technical content and relative links;
the website repository owns navigation chrome, rendering, deployment, analytics,
and other presentation concerns. Do not add site-framework configuration here.
