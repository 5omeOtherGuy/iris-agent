# Release and Update

Iris is a pre-release Rust CLI. Source installs work today. Prebuilt binary
release plumbing exists and becomes usable when release assets exist.

## Source install

```bash
cargo install --git https://github.com/5omeOtherGuy/iris-agent.git iris-agent --locked
```

From a checkout:

```bash
cargo run
```

## Runtime dependencies

The installed binary does not require external `rg` or `fd` binaries. Search and
file discovery use Rust libraries in process.

## Self-update

```bash
iris update
```

Prebuilt release binaries update from GitHub release assets and verify checksums.
Source-built binaries fall back to `cargo install`.

The self-replace path is compiled only for dist builds that carry both the
`self-update` feature and the `iris_dist` build marker. Other builds use the
cargo fallback even if compiled with all features.

## Release boundary

Releases require explicit human approval in the current turn. Do not push `v*`
tags, publish GitHub releases, configure crates.io tokens, or publish to crates.io
without operator approval.

The release-plz flow opens version/changelog PRs only. The operator pushes the
version tag by hand so the cargo-dist workflow builds archives, checksums, and
the installer, then creates the GitHub release.

Expected prebuilt assets are:

- `iris-agent-x86_64-unknown-linux-gnu.tar.gz` and `.sha256`
- `iris-agent-aarch64-unknown-linux-gnu.tar.gz` and `.sha256`
- `iris-agent-x86_64-apple-darwin.tar.gz` and `.sha256`
- `iris-agent-aarch64-apple-darwin.tar.gz` and `.sha256`
- `iris-agent-installer.sh`

## Validation

Before release, run:

```bash
bash scripts/validate-dist.sh
```

This validates `install.sh` and `iris update` against real archives and
checksums.
