# Release and Update

Iris is pre-1.0, but published releases and prebuilt archives are available.
Latest release checked on 2026-09-08: `v0.3.7` (2026-07-14). Main can contain
newer behavior than the installed release.

## Install

For a published crates.io release:

```bash
cargo install iris-agent --locked
```

For checksum-verified Linux/macOS archives, use the
[installer instructions](../README.md#install). Installing from Git main instead
selects unreleased source:

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

Release-plz prepares version/changelog PRs and publishes crates through the
operator-approved release flow. The operator pushes the version tag separately
so cargo-dist builds archives, checksums and the installer as a draft GitHub
release. Follow [RELEASING.md](../docs/RELEASING.md); preparation does not authorize
publication.

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
