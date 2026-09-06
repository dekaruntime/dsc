# Publishing dsc

Same loop as the runtime (`dekaruntime/deka`): **PR → merge → tag**. Never push
commits to `main`. A `v*` tag on `main` is what cuts a release.

## Cutting a release

1. On a branch, bump the lockstep version:
   ```bash
   ./scripts/bump-version.sh minor    # or patch, or an explicit X.Y.Z
   ```
2. Open a PR with the bump (and the work it belongs to). Merge it to `main`
   with a **merge commit** after **Rust tests** is green.
3. Tag from `main`, not from the feature branch:
   ```bash
   git checkout main
   git pull origin main
   scripts/dsc-version.sh              # must match the version you are tagging
   git tag -a "v$(scripts/dsc-version.sh)" -m "dsc v$(scripts/dsc-version.sh)"
   git push origin "v$(scripts/dsc-version.sh)"
   ```
4. Watch:
   ```bash
   gh run list --repo dekaruntime/dsc --workflow=release.yml
   gh run watch <RUN_ID> --repo dekaruntime/dsc --exit-status
   ```

The Release workflow aborts if the tag and `[workspace.package]` disagree.

## What the tag builds

`.github/workflows/release.yml`:

1. Tests the workspace (no V8).
2. Builds `dsc.wasm` / `dsc_diagnostics.wasm`.
3. Builds `dsc` CLI binaries for linux-x64, darwin-x64, darwin-arm64.
4. Uploads versioned artifacts to R2 (`dsc-releases`, `dsc-wasm`).
5. Promotes `latest` pointers.
6. Creates the GitHub release.

Public wasm host: `https://dsc-wasm.deka.gg`.

| Artifact | URL |
|---|---|
| Compiler WASM | `https://dsc-wasm.deka.gg/latest/dsc.wasm` |
| Diagnostics WASM | `https://dsc-wasm.deka.gg/latest/dsc_diagnostics.wasm` |
| Manifest | `https://dsc-wasm.deka.gg/latest/manifest.json` |
| Versioned | `https://dsc-wasm.deka.gg/v<VERSION>/...` |

Org R2 secrets (`R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`) must be allowed on
`dekaruntime/dsc` or the GitHub release is created and R2 is skipped. Allow the
org secrets (same keys as `deka`) for `https://dsc-wasm.deka.gg`.
