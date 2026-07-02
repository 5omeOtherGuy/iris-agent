#!/bin/sh
# Iris installer: download the latest prebuilt binary for this platform, verify
# its SHA-256 checksum, and install it. No Rust toolchain required.
#
#   curl -fsSL https://raw.githubusercontent.com/5omeOtherGuy/iris-agent/main/install.sh | sh
#
# Environment overrides:
#   IRIS_INSTALL_DIR       install directory (default: $CARGO_HOME/bin or ~/.local/bin)
#   IRIS_VERSION           release tag to install (default: latest)
#   IRIS_RELEASE_BASE_URL  artifact base URL (default: the GitHub release download
#                          URL for the resolved tag). Trust override: the archive
#                          AND its checksum are fetched from this base, so it
#                          delegates install authenticity to that host. Intended
#                          for local validation, not untrusted remote mirrors.
#
# POSIX sh; no bashisms. Artifact names match the cargo-dist config in
# Cargo.toml, which names archives after the cargo package (`iris-agent`), not
# the binary (`iris`): `iris-agent-<target>.tar.gz` + `.tar.gz.sha256`.
set -eu

REPO="5omeOtherGuy/iris-agent"
# Package name, used for the release archive filename (cargo-dist names archives
# after the package). The binary inside the archive is $BIN.
PKG="iris-agent"
BIN="iris"

say() { printf 'install: %s\n' "$1" >&2; }
err() {
	printf 'install: error: %s\n' "$1" >&2
	exit 1
}
need() { command -v "$1" >/dev/null 2>&1 || err "missing required command: $1"; }

detect_target() {
	os=$(uname -s)
	arch=$(uname -m)
	case "$os" in
	Linux) os_part="unknown-linux-gnu" ;;
	Darwin) os_part="apple-darwin" ;;
	*) err "unsupported OS: $os (install from source with cargo instead)" ;;
	esac
	case "$arch" in
	x86_64 | amd64) arch_part="x86_64" ;;
	arm64 | aarch64) arch_part="aarch64" ;;
	*) err "unsupported architecture: $arch" ;;
	esac
	printf '%s-%s' "$arch_part" "$os_part"
}

# Resolve the install directory: explicit override, else CARGO_HOME/bin, else
# ~/.local/bin. Matches the self-update path, which replaces the binary in place.
install_dir() {
	if [ -n "${IRIS_INSTALL_DIR:-}" ]; then
		printf '%s' "$IRIS_INSTALL_DIR"
	elif [ -n "${CARGO_HOME:-}" ]; then
		printf '%s/bin' "$CARGO_HOME"
	elif [ -d "$HOME/.cargo/bin" ]; then
		printf '%s/.cargo/bin' "$HOME"
	else
		printf '%s/.local/bin' "$HOME"
	fi
}

# Fetch a URL to a file. Prefer curl, fall back to wget.
fetch() {
	url="$1"
	out="$2"
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL "$url" -o "$out"
	elif command -v wget >/dev/null 2>&1; then
		wget -qO "$out" "$url"
	else
		err "need curl or wget to download"
	fi
}

resolve_version() {
	if [ -n "${IRIS_VERSION:-}" ]; then
		printf '%s' "$IRIS_VERSION"
		return
	fi
	api="https://api.github.com/repos/$REPO/releases/latest"
	tmp_json=$(mktemp)
	fetch "$api" "$tmp_json"
	# Extract "tag_name": "vX.Y.Z" without requiring jq.
	tag=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$tmp_json" | head -n1)
	rm -f "$tmp_json"
	[ -n "$tag" ] || err "could not determine the latest release tag"
	printf '%s' "$tag"
}

# POSIX sh functions have no local scope, so this uses underscore-prefixed
# variable names that do not collide with main()'s `archive`/`base`/etc.
# (assigning a bare `archive` here would clobber main's and corrupt the later
# `tar -xzf "$workdir/$archive"`).
verify_checksum() {
	_vc_archive="$1"
	_vc_sum_file="$2"
	_vc_expected=$(awk '{print $1; exit}' "$_vc_sum_file")
	[ -n "$_vc_expected" ] || err "empty checksum file"
	if command -v sha256sum >/dev/null 2>&1; then
		_vc_actual=$(sha256sum "$_vc_archive" | awk '{print $1}')
	elif command -v shasum >/dev/null 2>&1; then
		_vc_actual=$(shasum -a 256 "$_vc_archive" | awk '{print $1}')
	else
		err "need sha256sum or shasum to verify the download"
	fi
	[ "$_vc_actual" = "$_vc_expected" ] || err "checksum mismatch (expected $_vc_expected, got $_vc_actual)"
}

main() {
	need uname
	need tar
	need mktemp

	target=$(detect_target)
	version=$(resolve_version)
	dir=$(install_dir)
	archive="$PKG-$target.tar.gz"
	# Base directory that holds the archive + checksum. Defaults to the GitHub
	# release download URL; an override points at a local server (trailing slash
	# trimmed so "$base/$archive" is well-formed either way).
	base="${IRIS_RELEASE_BASE_URL:-https://github.com/$REPO/releases/download/$version}"
	base="${base%/}"

	say "installing $BIN $version ($target) to $dir"

	workdir=$(mktemp -d)
	trap 'rm -rf "$workdir"' EXIT INT TERM

	fetch "$base/$archive" "$workdir/$archive"
	fetch "$base/$archive.sha256" "$workdir/$archive.sha256"
	verify_checksum "$workdir/$archive" "$workdir/$archive.sha256"

	tar -xzf "$workdir/$archive" -C "$workdir"
	found=$(find "$workdir" -type f -name "$BIN" | head -n1)
	[ -n "$found" ] || err "binary '$BIN' not found in archive"

	# Stage the verified binary into a temp file inside the install directory so
	# the final install is an atomic same-filesystem rename. Moving straight from
	# the mktemp workdir can cross filesystems (copy+unlink), where an interrupt
	# could leave a truncated binary at the destination.
	mkdir -p "$dir"
	staged="$dir/.$BIN.tmp.$$"
	trap 'rm -rf "$workdir"; rm -f "$staged"' EXIT INT TERM
	cp "$found" "$staged"
	chmod +x "$staged"
	mv -f "$staged" "$dir/$BIN"

	say "installed $dir/$BIN"
	case ":$PATH:" in
	*":$dir:"*) : ;;
	*) say "note: $dir is not on PATH; add it to use '$BIN' directly" ;;
	esac
}

main "$@"
