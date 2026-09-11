#!/usr/bin/env bash
# Seed the @deka/io package cache the tour runner restores from, using a deka
# CLI (dsc is a compiler with no package manager, so it cannot `add` itself).
#
#   scripts/ci-seed-tour-io.sh /path/to/deka
#
# The tour runner (`.cache/tour/run.mjs`, owned by dekaruntime/tour) installs
# `@deka/io` into each lesson project from `<repo>/.cache/deka-packages/io` on
# a cache hit, fully offline. This script fills that cache once, so dsc can
# then compile every tour lesson — including the ones importing `io` — with a
# plain `DSC=<dsc> bun .cache/tour/run.mjs`. Idempotent: an existing cache is
# left alone.
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 DEKA_BINARY" >&2
  exit 2
fi
DEKA=$1
[[ -x "$DEKA" ]] || { echo "fatal: not executable: $DEKA" >&2; exit 2; }

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cache="$root/.cache/deka-packages/io"

if [[ -f "$cache/deka.lock" ]] && { [[ -d "$cache/ds_modules" ]] || [[ -d "$cache/php_modules" ]]; }; then
  echo "tour io cache already seeded: $cache"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Same project shape the tour runner installs into.
cat > "$tmp/deka.json" <<'JSON'
{
  "name": "tour-lesson",
  "security": {
    "allow": {
      "read": ["./"],
      "write": [".cache", "php_modules", "ds_modules"]
    },
    "prompt": false
  }
}
JSON
printf '{\n  "lockfileVersion": 1,\n  "packages": {}\n}\n' > "$tmp/deka.lock"

(cd "$tmp" && DEKA_SECURITY_NO_PROMPT=1 "$DEKA" add io --yes)

mkdir -p "$cache"
for modules in ds_modules php_modules; do
  if [[ -d "$tmp/$modules" ]]; then
    rm -rf "$cache/$modules"
    cp -R "$tmp/$modules" "$cache/$modules"
  fi
done
[[ -f "$tmp/deka.lock" ]] && cp "$tmp/deka.lock" "$cache/deka.lock"
[[ -d "$cache/ds_modules" || -d "$cache/php_modules" ]] \
  || { echo "fatal: 'deka add io' produced no modules" >&2; exit 1; }
echo "seeded tour io cache at $cache"
