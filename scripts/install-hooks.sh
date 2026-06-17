#!/usr/bin/env bash
# One-time setup: point git at the repo-tracked hooks in .githooks/.
# Run once per clone (config is shared across all worktrees of the clone).
#
#   bash scripts/install-hooks.sh
#
# Undo with: git config --unset core.hooksPath

set -euo pipefail

REPO_TOP=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "install-hooks: not inside a git repository" >&2
  exit 1
}
cd "$REPO_TOP"

chmod +x .githooks/* scripts/*.sh 2>/dev/null || true
git config core.hooksPath .githooks
echo "install-hooks: core.hooksPath set to .githooks (pre-commit, pre-push active)"
