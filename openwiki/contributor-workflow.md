# Contributor Workflow

All repository work happens in a per-task worktree. The primary checkout is
control-only and should stay aligned with `origin/main`.

## Worktree loop

From the primary checkout:

```bash
bash scripts/worktree-create.sh ../iris-<slug> <branch>
```

The wrapper runs freshness preflight and copies supported ignored local
instructions. Raw `git worktree add` does not copy them. Use
`bash scripts/worktree-preflight.sh` for a check without creation.

Work in the task worktree. Before opening an operator-approved PR:

```bash
bash scripts/gate.sh
```

Code changes run format, Clippy, tests and maintenance checks. Changes
[classified docs-only](../scripts/change-scope.sh) run whitespace validation,
not Rust tests or link checking; root agent guidance and nested crate READMEs
still trigger the full gate. Check documentation links and cited source paths
separately. A green local gate does not prove live
providers or distribution artifacts.

For TUI-focused work, `scripts/tui-live.sh` and `scripts/record-demo.sh` support
manual terminal testing. `docs/TUI_LIVE_TESTING.md` documents the workflow.

## Merge and cleanup

Only after explicit operator approval for the remote action:

```bash
gh pr merge <N> --squash --auto --delete-branch
bash scripts/worktree-cleanup.sh ../iris-<slug>
```

If the primary checkout drifts:

```bash
bash scripts/sync-primary.sh
```

## Documentation workflow

OpenWiki CLI setup exists for API-key provider usage:

```bash
bash scripts/openwiki-init.sh
bash scripts/openwiki-update.sh
```

For Codex-subscription-backed updates, use:

```bash
bash scripts/openwiki-codex-update.sh
```

If automation is blocked, edit `openwiki/` directly from the local source of
truth and review the diff like any other documentation change.

OpenWiki content must stay under `openwiki/`. Do not edit website files, release
files, Cargo metadata, or source code for a docs-only OpenWiki refresh.

## Website import

The separate website repo imports this directory:

```bash
cd <website-checkout>
npm run import:openwiki
npm run build
```

## Review focus

Blocking concerns:

- Workspace path traversal.
- Shell-command policy bypass.
- Approval-gate bypass.
- Tier-boundary leaks between Nexus, Wayland, Iris, and Mimir.
- Session, checkpoint, rollback, or permission-store data loss.
- Behavior changes without tests.
