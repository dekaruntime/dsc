#!/usr/bin/env bash
# Enable sccache when the binary and R2 keys are both present.
# Empty org secrets make RUSTC_WRAPPER=sccache fail every rustc.
set -euo pipefail

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  BUCKET=dsc-sccache-darwin-arm64 ;;
  Darwin-x86_64) BUCKET=dsc-sccache-darwin-x64 ;;
  Linux-x86_64)  BUCKET=dsc-sccache-linux-x64 ;;
  *) echo "unsupported sccache host: $(uname -sm)" >&2; exit 1 ;;
esac

if ! command -v sccache >/dev/null 2>&1; then
  echo "::warning::sccache not found on PATH - building without a compiler cache."
elif [ -z "${AWS_ACCESS_KEY_ID:-}" ] || [ -z "${AWS_SECRET_ACCESS_KEY:-}" ]; then
  echo "::warning::R2_ACCESS_KEY_ID/SECRET empty - building without sccache. Allow the org secrets on dekaruntime/dsc."
else
  if [ -n "${GITHUB_ENV:-}" ]; then
    echo "SCCACHE_BUCKET=$BUCKET" >> "$GITHUB_ENV"
    echo "RUSTC_WRAPPER=sccache" >> "$GITHUB_ENV"
  fi
  export SCCACHE_BUCKET="$BUCKET"
  echo "sccache enabled, bucket: $BUCKET"
  sccache --show-stats
fi
