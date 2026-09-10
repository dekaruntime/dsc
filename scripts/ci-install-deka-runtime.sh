#!/usr/bin/env bash
# Download the pinned deka runtime CLI for CI so the owner corpus runner
# (Hats) can execute fixtures with a real `deka run`.
#
#   scripts/ci-install-deka-runtime.sh /path/to/deka
#
# dsc cannot build the runtime itself (this repo must not depend on V8), so
# the gate runs against a released runtime, pinned in
# scripts/deka-runtime-version. Bump the pin in its own PR; that bump PR is
# where fixture-vs-runtime mismatches surface and get fixed. Artifacts come
# from https://releases.deka.gg (public, same layout as dsc-wasm.deka.gg).
# Checksums come from that host's per-version release.json. A missing file or
# a mismatch is a hard fail.
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 DEST_DEKA" >&2
  exit 2
fi

DEST=$1

VERSION_FILE="$(dirname "${BASH_SOURCE[0]}")/deka-runtime-version"
[[ -f "$VERSION_FILE" ]] || { echo "fatal: missing version pin: $VERSION_FILE" >&2; exit 1; }
VERSION="$(tr -d '[:space:]' < "$VERSION_FILE")"
[[ -n "$VERSION" ]] || { echo "fatal: $VERSION_FILE is empty" >&2; exit 1; }

BASE="${DEKA_RELEASES_BASE_URL:-https://releases.deka.gg}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64|Darwin-aarch64) PLATFORM=darwin-arm64 ;;
  Darwin-x86_64)               PLATFORM=darwin-x64 ;;
  Linux-x86_64)                PLATFORM=linux-x64 ;;
  *)
    echo "fatal: unsupported host $(uname -sm); deka publishes macOS arm64/x64 and Linux x64" >&2
    exit 1
    ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -fsSL "${BASE}/${VERSION}/release.json" -o "${tmp}/release.json"
expected=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["binaries"][sys.argv[2]]["sha256"])' "${tmp}/release.json" "$PLATFORM")
[[ -n "$expected" ]] || {
  echo "fatal: could not read deka ${PLATFORM} from ${BASE}/${VERSION}/release.json" >&2
  exit 1
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

mkdir -p "$(dirname "$DEST")"
curl -fsSL "${BASE}/${VERSION}/deka-${PLATFORM}" -o "${tmp}/deka"
actual=$(sha256_file "${tmp}/deka")
if [[ "$expected" != "$actual" ]]; then
  echo "fatal: deka checksum mismatch for ${PLATFORM} ${VERSION}" >&2
  echo "  expected ${expected}" >&2
  echo "  actual   ${actual}" >&2
  exit 1
fi
chmod 755 "${tmp}/deka"
mv -f "${tmp}/deka" "$DEST"
echo "installed deka ${VERSION} (${PLATFORM}) -> $DEST"
