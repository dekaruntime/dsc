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

Public wasm + CLI host: `https://dsc-wasm.deka.gg`.

| Artifact | URL |
|---|---|
| Compiler WASM | `https://dsc-wasm.deka.gg/latest/dsc.wasm` |
| Diagnostics WASM | `https://dsc-wasm.deka.gg/latest/dsc_diagnostics.wasm` |
| Manifest | `https://dsc-wasm.deka.gg/latest/manifest.json` |
| CLI (linux-x64) | `https://dsc-wasm.deka.gg/latest/dsc-linux-x64` |
| CLI (darwin-x64) | `https://dsc-wasm.deka.gg/latest/dsc-darwin-x64` |
| CLI (darwin-arm64) | `https://dsc-wasm.deka.gg/latest/dsc-darwin-arm64` |
| Versioned | `https://dsc-wasm.deka.gg/v<VERSION>/...` |

The final Publish job uses the `release` environment's `R2_ENDPOINT`,
`R2_ACCESS_KEY_ID`, and `R2_SECRET_ACCESS_KEY`. It fails before publishing if
any are unavailable; CI and release build jobs never receive those values.
