#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
extra_request="${*:-Refresh the Iris OpenWiki documentation.}"

prompt="$(cat <<'PROMPT'
You are maintaining Iris's offline wiki.

Write or update Markdown documentation under ./openwiki/ only. Treat ./openwiki/
as generated-but-reviewed documentation that should describe the repository as
implemented today.

Use the code and existing docs as sources of truth. When docs and code conflict,
trust the code and note stale source docs only when relevant.

Required output shape:
- ./openwiki/README.md: entry point and navigation.
- Focused topic pages for architecture, CLI usage, provider/auth setup, tools,
  sessions/storage, TUI behavior, release/update flow, and contributor workflow
  when those topics are supported by the current codebase.
- Keep writing terse and factual. Avoid future claims unless they are clearly
  marked as not implemented.

Do not edit website files, release files, Cargo metadata, or source code unless
the user explicitly asks for that in this run.
PROMPT
)"

cd "$repo_root"

exec codex exec \
  --cd "$repo_root" \
  --sandbox workspace-write \
  "${prompt}

User request: ${extra_request}"
