#!/usr/bin/env bash
# Build browser compiler + diagnostics WASM. No V8.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out_dir=${1:-"$root/dist/wasm"}
case "$out_dir" in
  /*) ;;
  *) out_dir="$root/$out_dir" ;;
esac
target_dir=${CARGO_TARGET_DIR:-"$root/target"}
source_commit=$(git -C "$root" rev-parse HEAD)
sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}
cargo_lock_sha256=$(sha256_file "$root/Cargo.lock")
version=$("$root/scripts/dsc-version.sh")

mkdir -p "$out_dir"
cd "$root"
CARGO_INCREMENTAL=0 DSC_SOURCE_COMMIT="$source_commit" \
  cargo build --locked --release --target wasm32-unknown-unknown -p deka_compiler_wasm
CARGO_INCREMENTAL=0 DSC_SOURCE_COMMIT="$source_commit" \
  cargo build --locked --release --target wasm32-unknown-unknown -p dekascript_lsp_wasm

cp "$target_dir/wasm32-unknown-unknown/release/deka_compiler_wasm.wasm" "$out_dir/dsc.wasm"
cp "$target_dir/wasm32-unknown-unknown/release/dekascript_lsp_wasm.wasm" "$out_dir/dsc_diagnostics.wasm"

compiler_sha=$(sha256_file "$out_dir/dsc.wasm")
diag_sha=$(sha256_file "$out_dir/dsc_diagnostics.wasm")
printf '%s  dsc.wasm\n' "$compiler_sha" > "$out_dir/dsc.wasm.sha256"
printf '%s  dsc_diagnostics.wasm\n' "$diag_sha" > "$out_dir/dsc_diagnostics.wasm.sha256"

cat > "$out_dir/manifest.json" <<EOF
{
  "schema_version": 1,
  "version": "$version",
  "source_commit": "$source_commit",
  "cargo_lock_sha256": "$cargo_lock_sha256",
  "wasm": {
    "compiler": "dsc.wasm",
    "compiler_sha256": "$compiler_sha",
    "diagnostics": "dsc_diagnostics.wasm",
    "diagnostics_sha256": "$diag_sha"
  }
}
EOF

printf 'Built %s and %s\n' "$out_dir/dsc.wasm" "$out_dir/dsc_diagnostics.wasm"
