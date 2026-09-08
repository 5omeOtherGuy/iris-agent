# Contributing

Thanks for improving Iris.

## Quick start

1. Create a task worktree using the workflow below; primary `main` is control-only.
2. Make the smallest focused change.
3. Write a failing deterministic test before production changes.
4. Run `bash scripts/gate.sh`. Code changes run format, Clippy, tests and
   maintenance checks; changes classified docs-only run whitespace checks. Root
   agent guidance and nested crate READMEs still trigger the full gate. Validate
   docs links/source references separately. Focus a test with `cargo test --locked <name>`.
5. Open a pull request into `main` when authorized by the operator.

## Pull request workflow

- Use GitHub issues for planned work when the scope is more than a small fix.
- Name branches after the work, for example `fix/cancel-race` or `docs/update-readme`.
- Keep the PR focused on one behavior or documentation change.
- Use the PR body to list summary bullets, verification commands, and follow-up work.
- Link related issues with `Closes #123` when the PR should close an issue on merge.
- Use labels to make release notes and triage easier: `bug`, `enhancement`, `documentation`, `security`, `dependencies`, `chore`, `tooling`, or `good first issue`.
- After checks finish, review failures with `gh pr checks` or `gh run view --log-failed`.

## Worktree workflow

Use a task-specific worktree for every repository change, including documentation.
Keep concurrent tasks out of one another's checkouts.

1. Install the repo hooks once per clone: `bash scripts/install-hooks.sh`. They block commits/pushes on a stale primary `main`. Reconcile drift with operator approval; do not bypass the hooks.
2. Create a worktree from the control-only primary checkout: `bash scripts/worktree-create.sh ../iris-<slug> <branch>`. The wrapper fetches and checks primary freshness, creates from `origin/main`, and copies only the supported ignored regular instruction files.
3. Optionally `export CARGO_TARGET_DIR=~/.cache/iris-target` so worktrees share build artifacts instead of each rebuilding `target/`.
4. Only after explicit operator approval, merge with `gh pr merge <N> --squash`, then clean up from outside the worktree: `bash scripts/worktree-cleanup.sh ../iris-<slug>` (removes the worktree, deletes the merged branch, and fast-forwards primary `main`).
5. Leave worktrees and branches you do not own untouched.

## Commit messages

Use Conventional Commits:

- `feat(scope): add new behavior`
- `fix(scope): correct broken behavior`
- `docs(scope): update documentation`
- `test(scope): add or update tests`
- `ci(scope): change GitHub Actions or automation`
- `chore(scope): maintain repo metadata or tooling`

Keep the summary imperative and under 72 characters. Add a body when it helps explain why the change exists or what trade-off it makes.

## Tests

Use deterministic local tests only. Do not require live provider/API calls for the default test suite. Prioritize tests for workspace path safety, tool result/error encoding, edit behavior, approval handling, and provider/tool cancellation.

## Security

Report vulnerabilities privately; see [SECURITY.md](SECURITY.md).
