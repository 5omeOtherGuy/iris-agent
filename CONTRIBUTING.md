# Contributing

Thanks for improving Iris.

## Quick start

1. Fork or branch from `main`.
2. Make the smallest focused change.
3. Add or update deterministic tests for behavior changes (TDD preferred).
4. Run the checks:
   - `cargo fmt --all --check` (`cargo fmt` applies fixes)
   - `cargo clippy --all-targets -- -D warnings`
   - `cargo test` (focus a single test with `cargo test <name>`)
5. Open a pull request into `main`.

## Pull request workflow

- Use GitHub issues for planned work when the scope is more than a small fix.
- Name branches after the work, for example `fix/cancel-race` or `docs/update-readme`.
- Keep the PR focused on one behavior or documentation change.
- Use the PR body to list summary bullets, verification commands, and follow-up work.
- Link related issues with `Closes #123` when the PR should close an issue on merge.
- Use labels to make release notes and triage easier: `bug`, `enhancement`, `documentation`, `security`, `dependencies`, `chore`, `tooling`, or `good first issue`.
- After checks finish, review failures with `gh pr checks` or `gh run view --log-failed`.

## Worktree workflow (for parallel work or multiple agents)

When more than one task or agent is active, isolate each in its own git worktree so concurrent work never collides in a shared checkout.

1. Install the repo hooks once per clone: `bash scripts/install-hooks.sh`. They block commits/pushes on a primary `main` that has drifted behind `origin/main` (bypass with `--no-verify` when you know why).
2. Preflight before branching: `bash scripts/worktree-preflight.sh` (fetches origin and checks primary freshness).
3. Create a worktree off `origin/main`: `git worktree add ../iris-<slug> -b <branch> origin/main`. The primary checkout is control-only -- never edit files or switch branches there.
4. Optionally `export CARGO_TARGET_DIR=~/.cache/iris-target` so worktrees share build artifacts instead of each rebuilding `target/`.
5. Merge with `gh pr merge <N> --squash`, then clean up from outside the worktree: `bash scripts/worktree-cleanup.sh ../iris-<slug>` (removes the worktree, deletes the merged branch, and fast-forwards primary `main`).
6. Leave worktrees and branches you do not own untouched.

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
