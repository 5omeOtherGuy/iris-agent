# Releasing Iris

How an operator cuts a public Iris release. Users install prebuilt binaries
(`install.sh`, `iris update` self-replace) or build from crates.io
(`cargo install iris-agent`); both channels release together. Two systems
cooperate:

- **release-plz** (`release-plz.toml`, `.github/workflows/release-plz.yml`) — on every
  push to `main` it opens/updates a release PR that bumps the version and maintains
  `CHANGELOG.md` from conventional commits. When that PR merges, the `release-plz-release`
  job publishes the crate to **crates.io** (gated on the `CARGO_REGISTRY_TOKEN` secret).
  It does **not** create the git tag or the GitHub release (`git_tag_enable = false`,
  `git_release_enable = false`).
- **cargo-dist** (`[workspace.metadata.dist]` in `Cargo.toml`,
  `.github/workflows/release.yml`) — triggered by the version tag an operator pushes. It
  builds the four prebuilt archives + SHA-256 checksums + the shell installer, then creates
  the GitHub release and attaches them. The release is created as a **draft** and published
  only after every asset is attached, so `releases/latest` can never resolve a
  half-populated release.

The operator pushes the tag by hand on purpose: a tag pushed by release-plz's default
`GITHUB_TOKEN` would not trigger `release.yml`
([release-plz docs](https://release-plz.dev/docs/github/token)).

## Status

Live. `v0.1.0` shipped 2026-07-09 (GitHub release with all nine assets) and
`iris-agent 0.1.0` is published on crates.io (the `CARGO_REGISTRY_TOKEN` secret is set,
`publish = true`).

## Version policy

release-plz computes the next version from conventional commits with
`features_always_increment_minor = true`: a `feat:` commit bumps the **minor** version even
on 0.x (`0.1.0 -> 0.2.0`); fix-only releases bump the patch. This is a deliberate product
choice — pre-1.0 Iris makes no API-stability promise, and the version exists to tell users
how much changed. The tag must equal `v` + the `Cargo.toml` version so `iris update`
matches it.

## Prerequisites

- Push access to `main` and permission to push tags / create releases.
- Green `main`: `bash scripts/gate.sh` plus the Lint workflow (actionlint, typos).

## Pre-release check (reproducible, no public action)

Prove the distribution paths before tagging. Requires a Rust toolchain and network; not
part of the gate.

```
bash scripts/validate-dist.sh
```

Builds the host archive (via `dist` if present, else a cargo+tar fallback), then exercises
the real `install.sh` download/verify/install path and the real `iris update`
download/verify/self-replace path, including checksum-mismatch refusal. Expect
`summary: N passed, 0 failed`.

The asset/checksum names and the `DIST_VERSION` = `cargo-dist-version` sync are locked by
unit tests in `src/selfupdate.rs`, so drift fails `cargo test`, not a release.

## Step 1 — cut the release **[operator]**

1. Merge the open **release-plz** PR (title `chore: release ...`). This bumps the version,
   finalizes `CHANGELOG.md` on `main`, and the `release-plz-release` job then publishes the
   new version to **crates.io**. No tag or GitHub release is created yet.
2. Push the version tag from the merged commit. It must equal `v` + the `Cargo.toml`
   version (e.g. `v0.2.0`) so `iris update` matches it:
   ```
   git switch main && git pull
   git tag v0.2.0
   git push origin v0.2.0
   ```
3. The tag triggers `.github/workflows/release.yml`. It builds the four targets, builds the
   installer, and creates the GitHub release (draft until all assets are attached, then
   published). Confirm the release has all nine files:
   - `iris-agent-x86_64-unknown-linux-gnu.tar.gz` + `.sha256`
   - `iris-agent-aarch64-unknown-linux-gnu.tar.gz` + `.sha256`
   - `iris-agent-x86_64-apple-darwin.tar.gz` + `.sha256`
   - `iris-agent-aarch64-apple-darwin.tar.gz` + `.sha256`
   - `iris-agent-installer.sh`
4. Confirm crates.io shows the new version (the `release-plz-release` job from step 1).

## Step 2 — post-release live acceptance **[operator]**

Run on a clean machine per platform (no prior Iris, no Rust toolchain needed):

```
curl -fsSL https://raw.githubusercontent.com/5omeOtherGuy/iris-agent/main/install.sh | sh
iris --help
```

Then confirm self-update from a prior build: install `vX.Y.Z-1`, run `iris update`, and
check it downloads, verifies the checksum, and self-replaces to `vX.Y.Z`. Both paths verify
SHA-256 before installing; a mismatch must abort.

## Testing channel — release candidates that never reach users

To exercise the whole pipeline (CI build, archives, installer, self-update) without
shipping anything to users, push a prerelease tag:

```
git tag v0.3.0-rc.1
git push origin v0.3.0-rc.1
```

`release.yml` builds it like any release but marks the release object **prerelease**, so it
never becomes `releases/latest` — `install.sh` and `iris update` both resolve latest and
cannot see it. `iris update` additionally refuses any tag with a semver prerelease
component (`decide_update`), a second, client-side gate. Acceptance-test an rc explicitly
with `IRIS_VERSION=v0.3.0-rc.1 ... install.sh` or by downloading its assets directly.
Delete rc releases/tags after the stable tag ships to keep the release page clean. Note
release-plz plays no role here: rc tags are cut from `main` directly, before the release
PR merges.

Day-to-day iteration needs none of this: `main` is never shipped — users only ever
receive operator-tagged stable releases, so merging to `main` is always safe.

## Maintaining the cargo-dist pipeline

`release.yml` is **hand-maintained**, not generated by `dist generate`: it pins `dist` via
`cargo install cargo-dist --locked --version` (supply-chain posture) and sets
`IRIS_DIST=1` on the build job so release binaries carry the `iris_dist` marker that
`iris update` self-replace requires. `[workspace.metadata.dist]` sets `allow-dirty = ["ci"]`
so `dist` accepts the divergence.

When bumping `cargo-dist-version` in `Cargo.toml`:

1. Update `DIST_VERSION` in `release.yml` to match (the sync test enforces equality).
2. Re-diff the canonical output and re-apply any needed changes by hand:
   `dist generate --mode ci --check` (with `allow-dirty` it reports the divergence without
   overwriting). Preserve the pinned install and `IRIS_DIST=1`.

## Failure and rollback

- **release.yml fails after the release exists:** fix forward and re-run the workflow; the
  tag and release already exist, so do not re-tag. Never force-push `main` or a tag.
- **Bad release:** mark the GitHub release as a draft/pre-release or delete its assets;
  cut a new patch version. A published crates.io version cannot be overwritten — yank it
  (`cargo yank --version X.Y.Z`) and publish a fix.
