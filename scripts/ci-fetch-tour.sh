#!/usr/bin/env bash
# Fetch the authoritative language tour for CI. The commit SHA is reviewed in
# a dedicated pin bump; SHA-256 verification prevents a changed archive from
# silently red-ing an unrelated language PR. The tour has no release tags yet,
# so the pin is a full commit SHA — immutable by construction, unlike a tag —
# and the checksum still makes any byte difference a hard failure.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
pin="$root/scripts/tour-version"
dest=${1:-"$root/.cache/tour"}
version=$(sed -n '1p' "$pin")
expected=$(sed -n '2p' "$pin")
[[ "$version" =~ ^[a-f0-9]{40}$ ]] || { echo "invalid tour commit in $pin" >&2; exit 2; }
[[ "$expected" =~ ^[a-f0-9]{64}$ ]] || { echo "invalid tour SHA-256 in $pin" >&2; exit 2; }

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}';
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
archive="$tmp/tour.tar.gz"
curl -fsSL "https://github.com/dekaruntime/tour/archive/${version}.tar.gz" -o "$archive"
actual=$(sha256_file "$archive")
[[ "$actual" == "$expected" ]] || { echo "tour checksum mismatch: expected $expected, got $actual" >&2; exit 1; }
tar -xzf "$archive" -C "$tmp"
source=$(find "$tmp" -mindepth 1 -maxdepth 1 -type d -name 'tour-*' | head -n 1)
[[ -n "$source" && -d "$source/tests/tour" ]] || { echo "archive does not contain tests/tour/" >&2; exit 1; }
rm -rf "$dest"
mkdir -p "$(dirname "$dest")"
mv "$source/tests/tour" "$dest"
test -f "$dest/manifest.json"
count=$(find "$dest" \( -name '*.ds' -o -name '*.dsx' \) | wc -l | tr -d ' ')
[[ "$count" -gt 80 ]] || { echo "tour is unexpectedly small: $count lessons" >&2; exit 1; }
echo "fetched dekaruntime/tour ${version:0:12} ($count lessons)"
