#!/usr/bin/env bash
# Reproducible local validation of the release distribution paths WITHOUT cutting
# a public release. Exercises the real code, not stubs:
#
#   1. Build the host-target artifact (iris-agent-<target>.tar.gz + .sha256) with
#      cargo-dist if `dist` is on PATH, else a cargo+tar fallback that reproduces
#      cargo-dist's archive layout and naming.
#   2. install.sh end-to-end over a local HTTP server (IRIS_RELEASE_BASE_URL):
#      happy path installs and runs; a corrupted .sha256 hard-fails.
#   3. `iris update` self-replace over a local mock of the GitHub releases API
#      (IRIS_UPDATE_RELEASES_API_URL, loopback-only): a newer tag downloads,
#      verifies, and self-replaces; an equal tag reports already-latest; a
#      corrupted checksum aborts without replacing.
#
# This is a manual, opt-in reproducer (network + a Rust toolchain required); it
# is deliberately NOT part of scripts/gate.sh. The invariants it depends on
# (asset/checksum names, DIST_VERSION sync) are locked by unit tests in
# src/selfupdate.rs so they fail the gate on drift.
#
# Usage: bash scripts/validate-dist.sh
set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
cd "$ROOT"
TARGET=$(rustc -vV | sed -n 's/^host: //p')
# The crate version the built binary reports; the equal-tag case below must
# offer exactly this version, or `iris update` correctly answers "ahead"
# instead of "already on the latest" and the check would rot on version bumps.
CUR_VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)
[ -n "$CUR_VERSION" ] || { echo "cannot read crate version from Cargo.toml" >&2; exit 1; }
ARC="iris-agent-$TARGET.tar.gz"
PORT_HTTP=8411
PORT_API=8412
PASS=0
FAIL=0
# SHA-256 of a file, portable across Linux (sha256sum) and macOS (shasum -a 256),
# matching install.sh's own fallback. Prints the bare hex digest.
sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{print $1}'
	else
		shasum -a 256 "$1" | awk '{print $1}'
	fi
}
# Write a `<hash>  <file>` sidecar for a file in the current directory, portable
# across sha256sum and shasum -a 256.
sha256_sidecar() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" >"$1.sha256"
	else
		shasum -a 256 "$1" >"$1.sha256"
	fi
}
ok() {
	printf '  PASS: %s\n' "$1"
	PASS=$((PASS + 1))
}
bad() {
	printf '  FAIL: %s\n' "$1"
	FAIL=$((FAIL + 1))
}

WORK=$(mktemp -d)
SERVERS=()
# Free our ports in case a previous interrupted run leaked a server (kill by
# port, never by `pkill -f http.server` which would also match this script).
for p in "$PORT_HTTP" "$((PORT_HTTP + 50))" "$PORT_API"; do fuser -k "$p/tcp" 2>/dev/null || true; done
cleanup() {
	for pid in "${SERVERS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
	rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

echo "== 1. Build host artifact ($TARGET) =="
if command -v dist >/dev/null 2>&1; then
	IRIS_DIST=1 dist build --artifacts=local --target="$TARGET" >/dev/null 2>&1 || {
		echo "dist build failed" >&2
		exit 1
	}
	DIST="$ROOT/target/distrib"
else
	echo "  dist not on PATH; cargo+tar fallback"
	IRIS_DIST=1 cargo build --release --features self-update >/dev/null 2>&1
	DIST="$WORK/distrib"
	mkdir -p "$DIST/iris-agent-$TARGET"
	cp target/release/iris "$DIST/iris-agent-$TARGET/iris"
	(cd "$DIST" && tar -czf "$ARC" "iris-agent-$TARGET/iris" && sha256_sidecar "$ARC")
fi
if [ -f "$DIST/$ARC" ] && [ -f "$DIST/$ARC.sha256" ]; then
	ok "artifact + checksum present: $ARC"
else
	bad "artifact or checksum missing"
fi
# Confirm the archive holds an `iris` binary and the sidecar matches real bytes.
# (Capture the listing first: `tar | grep -q` under pipefail fails on grep's
# early exit sending tar SIGPIPE.)
ARC_LIST=$(tar -tzf "$DIST/$ARC")
case "$ARC_LIST" in
*/iris | *"/iris"$'\n'*) ok "archive contains iris binary" ;;
*) bad "archive missing iris binary" ;;
esac
if [ "$(sha256 "$DIST/$ARC")" = "$(awk '{print $1}' "$DIST/$ARC.sha256")" ]; then
	ok "sha256 sidecar verifies over real archive bytes"
else
	bad "sha256 sidecar does not verify"
fi

echo
echo "== 2. install.sh (real download -> verify -> extract -> atomic install) =="
SERVE="$WORK/serve"
mkdir -p "$SERVE"
cp "$DIST/$ARC" "$DIST/$ARC.sha256" "$SERVE/"
# `exec` so the backgrounded subshell becomes python and $! is killable.
(cd "$SERVE" && exec python3 -m http.server "$PORT_HTTP" >/dev/null 2>&1) &
SERVERS+=($!)
sleep 1
BASE="http://127.0.0.1:$PORT_HTTP"

IDIR="$WORK/bin"
if IRIS_VERSION="v$CUR_VERSION" IRIS_RELEASE_BASE_URL="$BASE" IRIS_INSTALL_DIR="$IDIR" sh install.sh >/dev/null 2>&1 &&
	"$IDIR/iris" --help >/dev/null 2>&1; then
	ok "install.sh installed a runnable iris"
else
	bad "install.sh happy path"
fi

# Corrupted checksum must hard-fail.
BAD="$WORK/badserve"
mkdir -p "$BAD"
cp "$DIST/$ARC" "$BAD/"
echo "deadbeef  $ARC" >"$BAD/$ARC.sha256"
(cd "$BAD" && exec python3 -m http.server "$((PORT_HTTP + 50))" >/dev/null 2>&1) &
SERVERS+=($!)
sleep 1
if IRIS_VERSION="v$CUR_VERSION" IRIS_RELEASE_BASE_URL="http://127.0.0.1:$((PORT_HTTP + 50))" \
	IRIS_INSTALL_DIR="$WORK/bin_bad" sh install.sh >/dev/null 2>&1; then
	bad "corrupted checksum should have failed install.sh"
else
	[ ! -e "$WORK/bin_bad/iris" ] && ok "corrupted checksum hard-failed, nothing installed"
fi

echo
echo "== 3. iris update self-replace (real reqwest + sha256 + self_replace) =="
tar -xzf "$DIST/$ARC" -C "$WORK"
S=$(find "$WORK/iris-agent-$TARGET" -type f -name iris | head -n1)
S_SHA=$(sha256 "$S")
SPRIME="$WORK/iris_prime"
cp "$S" "$SPRIME"
printf '\n# self-update-test-sentinel\n' >>"$SPRIME"
chmod +x "$SPRIME"
SP_SHA=$(sha256 "$SPRIME")
API_SERVE="$WORK/apiserve"
mkdir -p "$API_SERVE/iris-agent-$TARGET"
cp "$SPRIME" "$API_SERVE/iris-agent-$TARGET/iris"
(cd "$API_SERVE" && tar -czf "$ARC" "iris-agent-$TARGET/iris" && sha256_sidecar "$ARC")
python3 - "$PORT_API" "$API_SERVE" "$ARC" >/dev/null 2>&1 <<'PY' &
import json, os, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
port, d, arc = int(sys.argv[1]), sys.argv[2], sys.argv[3]
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        if self.path == "/latest":
            tag = open(os.path.join(d, "TAG")).read().strip()
            base = f"http://127.0.0.1:{port}"
            body = json.dumps({"tag_name": tag, "assets": [
                {"name": arc, "browser_download_url": f"{base}/{arc}"},
                {"name": arc + ".sha256", "browser_download_url": f"{base}/{arc}.sha256"},
            ]}).encode()
            self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        p = os.path.join(d, self.path.lstrip("/"))
        if os.path.isfile(p):
            data = open(p, "rb").read()
            self.send_response(200); self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)
        else:
            self.send_response(404); self.end_headers()
HTTPServer(("127.0.0.1", port), H).serve_forever()
PY
SERVERS+=($!)
sleep 1
API="http://127.0.0.1:$PORT_API/latest"

# Case A: newer tag -> self-replace.
printf 'v9.9.9\n' >"$API_SERVE/TAG"
IA="$WORK/instA"
mkdir -p "$IA"
cp "$S" "$IA/iris"
IRIS_UPDATE_RELEASES_API_URL="$API" "$IA/iris" update >/dev/null 2>&1 || true
if [ "$(sha256 "$IA/iris")" = "$SP_SHA" ]; then
	ok "newer tag: downloaded, verified, self-replaced"
else
	bad "self-replace did not occur"
fi

# Case B: equal tag -> already latest, untouched.
printf 'v%s\n' "$CUR_VERSION" >"$API_SERVE/TAG"
IB="$WORK/instB"
mkdir -p "$IB"
cp "$S" "$IB/iris"
OUT=$(IRIS_UPDATE_RELEASES_API_URL="$API" "$IB/iris" update 2>&1 || true)
B_SHA=$(sha256 "$IB/iris")
if printf '%s' "$OUT" | grep -qi "already on the latest" && [ "$B_SHA" = "$S_SHA" ]; then
	ok "equal tag: already-latest, binary untouched"
else
	bad "already-latest case"
fi

# Case C: corrupted checksum -> abort, untouched.
printf 'v9.9.9\n' >"$API_SERVE/TAG"
printf 'deadbeef  %s\n' "$ARC" >"$API_SERVE/$ARC.sha256"
IC="$WORK/instC"
mkdir -p "$IC"
cp "$S" "$IC/iris"
IRIS_UPDATE_RELEASES_API_URL="$API" "$IC/iris" update >/dev/null 2>&1 || true
if [ "$(sha256 "$IC/iris")" = "$S_SHA" ]; then
	ok "corrupted checksum: aborted, binary unchanged"
else
	bad "corrupted checksum replaced anyway"
fi

echo
echo "== summary: $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
