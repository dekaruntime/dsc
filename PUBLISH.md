# Publishing dsc

[rfd#68](https://github.com/dekaruntime/rfd/issues/68): every build is a
canary, stable is a promotion of the same bytes. Never push commits to
`main`. **PR → merge → automatic canary → test the canary → promote.**

## Cutting a release

1. On a branch, bump the lockstep version:
   ```bash
   ./scripts/bump-version.sh minor    # or patch, or an explicit X.Y.Z
   ```
2. Open a PR with the bump (and the work it belongs to). Merge it to `main`
   with a **merge commit** after **Rust tests** is green.
3. The merge is the trigger. `tag-canary.yml` tags the merge commit
   `v<VERSION>-canary-<7-char-sha>`, pushes it, and starts
   `release.yml` against that tag automatically — no manual tag, no manual
   `gh workflow run`. It publishes to R2 under `v<VERSION>-canary-<sha>/`
   and the `canary/` pointers, and (once `create-deka-app` supports it) npm's
   `canary` dist-tag.
4. Watch it build:
   ```bash
   gh run list --repo dekaruntime/dsc --workflow=release.yml
   gh run watch <RUN_ID> --repo dekaruntime/dsc --exit-status
   ```
5. Test the canary — Sami's manual rounds, the tour against the canary
   build, testsuite-site, whatever this release needs. Nothing about the
   canary being published makes it `latest`; users on `latest` never see it
   until it's promoted.
6. **Promote it.** In the GitHub UI: **Actions → Promote → Run workflow**,
   paste the canary tag (e.g. `v0.58.3-canary-a1b2c3d`) into the `canary`
   input, run it against the `release` environment. See "Promotion, step by
   step" below for exactly what that does.

A red canary costs nothing but the canary tag: fix forward, merge again, get
a new canary at the same base version (`0.58.3-canary-<new-sha>`) — no
version bump required until one canary is actually promoted.

## What a canary build does

`.github/workflows/release.yml`, triggered by `tag-canary.yml` against a
`vX.Y.Z-canary-<sha>` ref:

1. Resolves and validates the ref (base version matches the workspace,
   embedded sha matches the tagged commit). A plain `vX.Y.Z` ref is refused
   — stable tags come from `promote.yml`, not from this workflow.
2. Builds `dsc.wasm` / `dsc_diagnostics.wasm`. No unit-test job runs here —
   PR CI (`ci.yml`) already gated the exact merge commit this canary is
   built from.
3. Builds `dsc` CLI binaries for linux-x64, darwin-x64, darwin-arm64.
4. Smoke-tests each built binary on its own platform: `dsc --version` prints
   the base version, `dsc check` accepts a valid one-line script and rejects
   an invalid one. Publish only runs if every platform passes.
5. Uploads versioned artifacts to R2 (`dsc-releases`, `dsc-wasm`) under
   `v<VERSION>-canary-<sha>/`, and updates the `canary/` pointers (never
   `latest/`).
6. Creates a **prerelease** GitHub release.
7. Dispatches the npm publish with `channel: canary` (see npm packages
   below).

Public wasm + CLI host: `https://dsc-wasm.deka.gg`.

| Artifact | URL |
|---|---|
| Compiler WASM (stable) | `https://dsc-wasm.deka.gg/latest/dsc.wasm` |
| Compiler WASM (newest canary) | `https://dsc-wasm.deka.gg/canary/dsc.wasm` |
| Diagnostics WASM | `https://dsc-wasm.deka.gg/latest/dsc_diagnostics.wasm` |
| Manifest | `https://dsc-wasm.deka.gg/latest/manifest.json` |
| CLI (linux-x64) | `https://dsc-wasm.deka.gg/latest/dsc-linux-x64` |
| CLI (darwin-x64) | `https://dsc-wasm.deka.gg/latest/dsc-darwin-x64` |
| CLI (darwin-arm64) | `https://dsc-wasm.deka.gg/latest/dsc-darwin-arm64` |
| Versioned (stable) | `https://dsc-wasm.deka.gg/v<VERSION>/...` |
| Versioned (canary) | `https://dsc-wasm.deka.gg/v<VERSION>-canary-<sha>/...` |

The Publish job uses the `release` environment's `R2_ENDPOINT`,
`R2_ACCESS_KEY_ID`, and `R2_SECRET_ACCESS_KEY`. It fails before publishing if
any are unavailable; CI and build jobs never receive those values.

## Promotion, step by step

`.github/workflows/promote.yml`, run manually with a canary tag as input.
`ubuntu-latest`, no Rust toolchain — this never rebuilds anything:

1. Validates the input: it matches `vX.Y.Z-canary-<sha>`, the canary tag
   exists, and `vX.Y.Z` does not already exist.
2. Downloads the canary's `release.json` from R2 and checks its `commit`
   field against the canary tag's actual commit — the bytes being promoted
   have to be the bytes that tag really points at.
3. `aws s3 cp --recursive`s the canary's R2 objects (both buckets) to the
   `vX.Y.Z/` path. No file is rebuilt or re-uploaded from a local artifact.
4. Rewrites `manifest.json` and `release.json` in place at the new path:
   `version`/`tag` become the stable pair, `channel` flips to `"stable"`,
   `promoted_from` records the canary tag. Every checksum and the `commit`
   field are untouched.
5. Creates the annotated `vX.Y.Z` tag **at the canary's commit** (not at
   whatever `main` is when the button is pressed) and pushes it.
6. Creates a GitHub release for `vX.Y.Z` — same as before, minus the
   prerelease flag.
7. Flips the `latest/` pointers (both buckets) to the new `vX.Y.Z/` files.
8. Dispatches the npm publish with `channel: stable`.

## Downstream

The only *direct* downstream of a dsc release is npm (see below):
`release.yml`'s and `promote.yml`'s `notify`/final steps send a
`repository_dispatch` (`dsc-released`, with `{tag, channel}`) to
`dekaruntime/create-deka-app`'s `publish-runtime.yml` using the org secret
`CASCADE_DISPATCH_TOKEN`; if that token is unset, the step fails loudly and
`create-deka-app`'s hourly fallback picks the release up within the hour
instead.

**Needs Sami / follow-up in `create-deka-app`:** `publish-runtime.yml` does
not yet branch on `channel`. Until it does, the `canary` dispatch payload is
delivered but npm's `canary` dist-tag is not actually published from it —
only the R2 `canary/` contract on the dsc side is live from this PR. Stable
promotion (`channel: stable`) is unaffected; it dispatches the same shape
`publish-runtime.yml` already consumes today.

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
