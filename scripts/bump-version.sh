#!/usr/bin/env bash
# Bump every workspace crate in lockstep.
#
# dsc has its own semver. Do not copy deka (runtime) version numbers.
# Line starts at 0.1.0 and iterates 0.2.0, 0.3.0, …
#
# Usage:
#   scripts/bump-version.sh patch|minor|major
#   scripts/bump-version.sh 0.2.0
#   scripts/bump-version.sh --print
#   scripts/bump-version.sh --dry-run minor
#
# Writes [workspace.package] version, every crates/*/Cargo.toml that still
# inlines a version, and Cargo.lock. Does not commit or tag.
#
# Tag from main AFTER this lands:
#   git tag -a "v$(scripts/dsc-version.sh)" -m "dsc v$(scripts/dsc-version.sh)"
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

usage() {
  sed -n '2,18p' "$0" | sed 's/^# \?//'
  exit 2
}

PRINT=0
DRY_RUN=0
TARGET=""
for arg in "$@"; do
  case "$arg" in
    --print) PRINT=1 ;;
    --dry-run) DRY_RUN=1 ;;
    -h|--help) usage ;;
    -*)
      echo "unknown flag: $arg" >&2
      usage
      ;;
    *)
      if [[ -n "$TARGET" ]]; then
        echo "unexpected extra argument: $arg" >&2
        usage
      fi
      TARGET="$arg"
      ;;
  esac
done

CURRENT="$(scripts/dsc-version.sh)"

if [[ "$PRINT" -eq 1 ]]; then
  echo "$CURRENT"
  exit 0
fi

if [[ -z "$TARGET" ]]; then
  usage
fi

parse_semver() {
  local v="$1"
  if [[ ! "$v" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
    echo "not a semver X.Y.Z: $v" >&2
    return 1
  fi
  printf '%s %s %s' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "${BASH_REMATCH[3]}"
}

semver_gt() {
  local a_maj a_min a_pat b_maj b_min b_pat
  read -r a_maj a_min a_pat <<<"$(parse_semver "$1")"
  read -r b_maj b_min b_pat <<<"$(parse_semver "$2")"
  if (( a_maj != b_maj )); then
    (( a_maj > b_maj ))
  elif (( a_min != b_min )); then
    (( a_min > b_min ))
  else
    (( a_pat > b_pat ))
  fi
}

next_from_bump() {
  local kind="$1"
  local maj min pat
  read -r maj min pat <<<"$(parse_semver "$CURRENT")"
  case "$kind" in
    patch) echo "${maj}.${min}.$((pat + 1))" ;;
    minor) echo "${maj}.$((min + 1)).0" ;;
    major)
      if [[ "$maj" == "0" ]]; then
        echo "1.0.0"
      else
        echo "$((maj + 1)).0.0"
      fi
      ;;
    *) return 1 ;;
  esac
}

NEW=""
case "$TARGET" in
  patch|minor|major) NEW="$(next_from_bump "$TARGET")" ;;
  *)
    parse_semver "$TARGET" >/dev/null
    NEW="$TARGET"
    ;;
esac

if [[ "$NEW" == "$CURRENT" ]]; then
  echo "already at $CURRENT" >&2
  exit 1
fi
if ! semver_gt "$NEW" "$CURRENT"; then
  echo "refusing to move $CURRENT -> $NEW (must increase)" >&2
  exit 1
fi
# dsc is a new 0.x line. deka (runtime) was on 0.42 when crates were copied;
# that number is not ours.
if [[ "$NEW" =~ ^0\.42\. ]]; then
  echo "refusing $NEW: 0.42.x is deka (runtime) versioning, not dsc." >&2
  echo "dsc starts at 0.1.0 and iterates 0.2.0, 0.3.0, …" >&2
  exit 1
fi

LATEST_TAG="$(git tag -l 'v[0-9]*' --sort=-v:refname | head -n1 || true)"
if [[ -n "$LATEST_TAG" ]]; then
  LATEST_VER="${LATEST_TAG#v}"
  if [[ "$LATEST_VER" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] && ! semver_gt "$NEW" "$LATEST_VER"; then
    echo "refusing $NEW: latest git tag is ${LATEST_TAG}." >&2
    echo "the next number has to clear the published tag, not just the crate files." >&2
    echo "pass an explicit version greater than ${LATEST_VER}." >&2
    exit 1
  fi
fi

if git rev-parse "v${NEW}" >/dev/null 2>&1; then
  echo "refusing $NEW: git tag v${NEW} already exists" >&2
  exit 1
fi

echo "$CURRENT -> $NEW"

if [[ "$DRY_RUN" -eq 1 ]]; then
  exit 0
fi

python3 - "$ROOT_DIR" "$NEW" <<'PY'
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
new = sys.argv[2]

ws = root / "Cargo.toml"
text = ws.read_text()
new_text, n = re.subn(
    r"(?ms)(^\[workspace\.package\]\s*)(.*?)(?=^\[|\Z)",
    lambda m: m.group(1) + re.sub(r'(?m)^version\s*=\s*"[^"]+"', f'version = "{new}"', m.group(2), count=1),
    text,
    count=1,
)
if n != 1:
    sys.stderr.write("failed to update [workspace.package] version\n")
    sys.exit(1)
ws.write_text(new_text)

crate_re = re.compile(r'(?m)^version\s*=\s*"[^"]+"')
for path in sorted((root / "crates").glob("*/Cargo.toml")):
    body = path.read_text()
    if re.search(r"(?m)^version\.workspace\s*=\s*true\s*$", body):
        continue
    updated, n = crate_re.subn(f'version = "{new}"', body, count=1)
    if n:
        path.write_text(updated)
        print(f"updated {path.relative_to(root)}")
PY

if command -v cargo >/dev/null 2>&1; then
  cargo update --workspace --offline 2>/dev/null || cargo update --workspace
fi

echo
echo "tree is now $NEW. Open a PR with this bump, merge it, then from main:"
echo "  git tag -a v${NEW} -m \"dsc v${NEW}\""
echo "  git push origin v${NEW}"
echo
echo "Confirm before tagging:"
echo "  scripts/dsc-version.sh   # must print ${NEW}"
