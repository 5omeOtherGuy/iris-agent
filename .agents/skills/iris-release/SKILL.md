---
name: iris-release
description: Cut and verify a public iris-agent release (crates.io + prebuilt binaries). Use when asked to release iris, cut/tag a new version, merge the rolling release PR, or verify/finish a release that failed midway. Canonical runbook is docs/RELEASING.md; this skill adds the operational hardening learned from live cuts.
---

# Releasing iris-agent

docs/RELEASING.md is the canonical runbook — read it first; this skill layers
the operator procedure and the failure modes that RELEASING.md doesn't cover.

Two systems cooperate: **release-plz** (rolling `chore: release vX.Y.Z` PR;
merging it publishes to crates.io) and **cargo-dist** (`release.yml`, triggered
by the operator-pushed tag; builds 4 targets and creates the GitHub release
draft-first). release-plz never tags; the operator always pushes the tag.

## Ground rules

- All branch work happens in a **worktree** — never switch branches in the
  primary checkout.
- Never push commits onto the release-plz PR branch: the next release-plz run
  will CLOSE that PR and open a fresh one; curation gets clobbered.
- Agent-authored PRs (e.g. a policy or curation change) need explicit per-PR
  user approval before self-merging (merge classifier) — ask, one tap. The
  release-plz PR itself is bot-authored; a user's release request covers
  merging it, but its merge publishes to crates.io, so never merge it on your
  own initiative.
- Never force-push main or a tag. A published crates.io version can only be
  yanked, not replaced.
- **Freeze main during the cut**: any push to main between reviewing and
  merging the release PR regenerates its branch (checks vanish, review is
  stale). Don't merge other PRs mid-cut; after the release PR merges, tag
  promptly.

## Procedure

### 1. Pre-flight

```bash
git fetch origin && git status            # clean, up to date with origin/main
gh pr list --state open                   # only the release-plz PR should be open
gh pr list --state merged --limit 5       # confirm what's going into the release
git tag --sort=-v:refname | head -3       # what already shipped
grep -m1 '^version' Cargo.toml            # current version (root crate, not iris-bench)
```

- If the user names a version, check it against reality: it may already be
  released (tag exists), or conflict with the computed next version in the
  rolling release PR title. Surface the mismatch and get a decision before
  touching anything public.
- Version policy (since #603): `features_always_increment_minor = false` — on
  0.x, `feat:` bumps **patch**; minor is reserved for breaking changes.
- A dirty primary checkout does not itself block the release — CI builds from
  the tag, not from local files. Unrelated local edits (someone's work in
  progress): leave untouched and proceed. But run `validate-dist.sh` from a
  clean worktree in that case, so local edits can't skew the validation.
- **No open release PR?** Nothing merged since the last release means nothing
  to release. If commits did land, check `gh run list --workflow=release-plz.yml`
  for a failed run and `gh run rerun <id>` it. (The workflow triggers only on
  push to main — it has no workflow_dispatch, so you cannot start it manually.)

### 2. Pre-release validation (no public action)

```bash
bash scripts/validate-dist.sh    # expect: summary: N passed, 0 failed
```

Builds the host archive and exercises the real install.sh and `iris update`
paths including checksum-corruption refusal. Takes a few minutes (cargo build).

### 3. Review the release PR

```bash
gh pr view <N> --json title     # title must say the intended version
gh pr diff <N>                  # CHANGELOG completeness, Cargo.toml bump
```

Cross-check completeness: `git log vPREV..origin/main --oneline` — every
user-visible PR must appear in the changelog (deps/chore may be grouped under
"Other"). Curate only
if needed, as late as possible, on the PR branch (check out that branch in a
worktree — never in the primary checkout) — and merge immediately after (any
main push regenerates and clobbers the branch).

### 4. Merge the release PR

First run `gh pr checks <N>`. **Expect "no checks reported" whenever
release-plz has regenerated the branch (i.e. almost always):** release-plz
pushes with `GITHUB_TOKEN`, and GitHub never triggers workflows for those
pushes, so required checks don't exist on the new head and the merge is
BLOCKED by branch policy. Fix: `gh pr close <N> && sleep 3 && gh pr reopen <N>`
(operator token fires the pull_request events); confirm checks appeared
(`gh pr checks <N>`), wait for green, then merge.

```bash
gh pr checks <N> --watch
HEAD=$(gh pr view <N> --json headRefOid -q .headRefOid)   # pin what you reviewed
gh pr merge <N> --squash --subject "chore: release vX.Y.Z (#<N>)" \
  --match-head-commit "$HEAD"
```

`--match-head-commit` refuses the merge if release-plz regenerated the branch
after your review (any concurrent main push does that) — re-review, don't force.

- Pass `--subject` explicitly: GitHub may prefill the squash title from a stale
  PR title (a v0.4.0→v0.3.2 regeneration kept the old title in the log).
- Don't pass `--delete-branch` for the release PR (release-plz manages its own
  branches). If any `gh pr merge` exits 1 from a worktree, the remote merge may
  still have succeeded (the local main checkout step fails) — verify with
  `gh pr view <N> --json state,mergeCommit` before retrying anything.
- Merging starts the `release-plz-release` job → crates.io publish. Watch it:
  `gh run list --workflow=release-plz.yml --limit 1`.

### 5. Tag → prebuilt binaries

```bash
V=X.Y.Z                               # the ONE place the version is written
git fetch origin
SHA=$(gh pr view <N> --json mergeCommit -q .mergeCommit.oid)
[ -n "$SHA" ] || { echo "no merge commit — PR not merged?"; exit 1; }
# invariant: tag == v + Cargo.toml version at that commit, or iris update breaks
git show "$SHA":Cargo.toml | grep -m1 '^version' | grep -qF "\"$V\"" \
  || { echo "version mismatch at $SHA"; exit 1; }
git tag "v$V" "$SHA"
git push origin "v$V"
```

This triggers release.yml (cargo-dist). Wait for it before verifying:

```bash
gh run watch $(gh run list --workflow=release.yml -L1 --json databaseId -q '.[0].databaseId') --exit-status
```

Expect 10–20 minutes (4 targets, aarch64-linux via zigbuild). If it fails
after the release object exists: fix forward and re-run the workflow; never
re-tag. (`gh release view` erroring with "release not found" mid-build is
normal — the release object appears late in the run.)

The waits in steps 4–5 run 2–20 minutes: use long command timeouts, background
tasks, or polling loops — don't let a default shell timeout kill a watch and
then misread the release as failed.

### 6. Verify

```bash
gh release view vX.Y.Z --json isDraft,isPrerelease   # both must be false
gh release view vX.Y.Z --json assets -q '.assets[].name'   # exactly 9 assets
curl -s -A "iris-release-check" https://crates.io/api/v1/crates/iris-agent \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["crate"]["max_version"])'
```

The crates.io API rejects requests without a User-Agent header.

9 assets = 4 `iris-agent-<target>.tar.gz` + 4 `.sha256` + `iris-agent-installer.sh`.

### 7. Acceptance (scratch HOME, current platform)

```bash
S=$(mktemp -d); HOME="$S" sh -c \
  'curl -fsSL https://raw.githubusercontent.com/5omeOtherGuy/iris-agent/main/install.sh | sh \
   && "$HOME/.local/bin/iris" --version'

S2=$(mktemp -d); HOME="$S2" sh -c \
  'curl -fsSL https://raw.githubusercontent.com/5omeOtherGuy/iris-agent/main/install.sh | IRIS_VERSION=vPREV sh \
   && "$HOME/.local/bin/iris" update && "$HOME/.local/bin/iris" --version'
```

`vPREV` = the previous stable tag (its release must still have assets; if it
was cleaned up, skip the self-update leg and note that in the report).

Expect: fresh install resolves the new tag with `sha-256 ok`; the previous
binary self-replaces `vPREV → vX.Y.Z`. Cross-platform (macOS/aarch64) legs run
only if such a machine is available — otherwise state that they were skipped.

### 8. Close out

Report to the user: versions on both channels, asset count, acceptance results,
and anything that deviated from this skill (then update this skill).

## Resuming a broken or half-finished cut

Determine which stage completed, then continue — never restart from scratch:

| Observation | State | Next action |
|---|---|---|
| Release PR merged, crates.io shows new version, no tag | step 5 pending | tag the merge commit, push |
| Tag exists, release.yml failed | step 5 broken | fix forward, re-run the workflow run; never re-tag |
| Release exists but draft / <9 assets | release.yml incomplete | re-run failed jobs; release publishes when all assets attach |
| crates.io missing the version but PR merged | release-plz-release job failed | inspect + re-run `release-plz.yml` run |
| Bad release shipped | rollback | draft/prerelease the GH release or delete assets; `cargo yank --version X.Y.Z`; cut a new patch |

## Testing channel

Prerelease tags (`vX.Y.Z-rc.N`, pushed from main before the release PR merges)
exercise the whole pipeline invisibly to users — see RELEASING.md. Delete rc
releases/tags after the stable ships.
