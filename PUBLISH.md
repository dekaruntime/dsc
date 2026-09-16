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
7. Dispatches the npm publish (see npm packages below).

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

## Downstream

The only *direct* downstream of a dsc release is npm (see below):
`.github/workflows/release.yml`'s `notify` job sends a `repository_dispatch`
(`dsc-released`) to `dekaruntime/create-deka-app`'s `publish-runtime.yml`
using the org secret `CASCADE_DISPATCH_TOKEN`; if that token is unset, the
step exits 0 and `create-deka-app`'s hourly fallback picks the release up
within the hour instead.

Everything else picks up a dsc version indirectly:

- **The tour** (`deka.gg`) tracks `dsc-wasm.deka.gg/latest` directly, via
  `dekaruntime/website`'s own `sync-deka-compiler.yml` (its own hourly cron,
  independent of `dekaruntime/deka`). A dsc release reaches the tour on its
  own, without a deka release involved.
- **`@dekaruntime/deka`, its platform packages, and anything else that
  consumes the deka runtime** only move to a new dsc version when a deka
  release bumps `scripts/dsc-version` and re-releases — see
  [`dekaruntime/deka`'s `PUBLISH.md`](https://github.com/dekaruntime/deka/blob/main/PUBLISH.md)
  for that waterfall.

## npm packages (downstream)

dsc is also published to npm, but not by this repo. All npm delivery lives in
one public repo, `dekaruntime/create-deka-app`, with a single workflow:
`.github/workflows/publish-runtime.yml`. That repo never compiles anything
and never commits versions or binaries — the workflow downloads the release
binaries from `https://dsc-wasm.deka.gg/v<VERSION>/` and
`latest/release.json`, verifies every sha256 against the release manifest,
stamps the version into `package.json` files in the working tree only, and
publishes via npm trusted publishing (OIDC, no stored token).

### Packages

| Package | Role |
|---|---|
| `@dekaruntime/dsc` | Launcher |
| `@dekaruntime/dsc-darwin-arm64` | Platform binary |
| `@dekaruntime/dsc-darwin-x64` | Platform binary |
| `@dekaruntime/dsc-linux-x64` | Platform binary |

dsc keeps its own version line on npm. `@dekaruntime/deka` pins
`@dekaruntime/dsc` at the exact version in deka's `scripts/dsc-version`, and
the deka platform packages bundle the dsc binary at that same pinned version,
so a dsc release never changes what an existing `deka`/`create-deka-app` user
has installed. A deka release requires its pinned dsc version to already be
on npm before it will publish anything itself.

### Trigger

1. The "Trigger npm package publish" step of `release.yml`'s `notify` job
   sends a `repository_dispatch` (`dsc-released`) to
   `dekaruntime/create-deka-app` using the org secret
   `CASCADE_DISPATCH_TOKEN`. `dekaruntime/dsc` is in that secret's Repository
   access list.
2. Fallback: `create-deka-app` runs `publish-runtime.yml` hourly
   (`17 * * * *`); it publishes whatever the release hosts have that npm
   doesn't, for both the dsc and deka families (dsc first), and is a no-op
   otherwise.
3. Manual:
   `gh workflow run publish-runtime.yml -R dekaruntime/create-deka-app -f family=dsc -f version=<X.Y.Z>`

### Guarantees

Every package in a run is pre-flighted (stamped version, no placeholder pins,
`npm pack` succeeds, binaries present in platform tarballs) before the first
`npm publish`, so one failure publishes nothing. Publish order is platform
packages → launcher, each read back from the registry, so users never see a
half release. A run that dies partway is completed by the next trigger —
publishing an already-published version is treated as done. Publishes go
straight to `latest`; the OIDC credential cannot run `npm dist-tag`.

### Verification

```bash
npm view @dekaruntime/dsc dist-tags.latest
```

Should equal the released version within ~10 minutes of the release job
finishing.

```bash
gh run list -R dekaruntime/create-deka-app --workflow publish-runtime.yml
```
