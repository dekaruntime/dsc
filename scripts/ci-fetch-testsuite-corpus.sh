#!/usr/bin/env bash
# Download the authoritative conformance corpus for CI. The tag identifies the
# reviewed corpus release; the SHA-256 makes a moved tag or altered response a
# hard failure rather than silently changing an unrelated compiler run.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
pin="$root/scripts/testsuite-corpus-version"
dest=${1:-"$root/.cache/testsuite-corpus"}

version=$(sed -n '1p' "$pin")
expected=$(sed -n '2p' "$pin")
[[ "$version" =~ ^corpus-v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "invalid corpus version: $version" >&2; exit 2; }
[[ "$expected" =~ ^[a-f0-9]{64}$ ]] || { echo "invalid corpus SHA-256 in $pin" >&2; exit 2; }

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}';
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
archive="$tmp/corpus.tar.gz"
url="${DEKA_TESTSUITE_ARCHIVE_URL:-https://github.com/dekaruntime/testsuite/archive/refs/tags/${version}.tar.gz}"
curl -fsSL "$url" -o "$archive"
actual=$(sha256_file "$archive")
[[ "$actual" == "$expected" ]] || { echo "testsuite corpus checksum mismatch: expected $expected, got $actual" >&2; exit 1; }
tar -xzf "$archive" -C "$tmp"
source=$(find "$tmp" -mindepth 1 -maxdepth 1 -type d -name 'testsuite-*' | head -n 1)
[[ -n "$source" && -d "$source/corpus" ]] || { echo "archive does not contain corpus/" >&2; exit 1; }
rm -rf "$dest"
mkdir -p "$(dirname "$dest")"
mv "$source/corpus" "$dest"
test -f "$dest/expected-failures.txt"
count=$(find "$dest" \( -name '*.pass.ds' -o -name '*.pass.dsx' \) | wc -l | tr -d ' ')
[[ "$count" -gt 100 ]] || { echo "corpus is unexpectedly small: $count pass fixtures" >&2; exit 1; }
echo "fetched testsuite corpus $version ($count pass fixtures)"
