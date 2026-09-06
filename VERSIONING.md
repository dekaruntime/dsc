# Versioning

dsc has its **own** semver. It does not inherit deka (runtime) version numbers.

Compiler crates in this tree were copied from deka while that repo was on a
`0.42.x` line. That number is not dsc's version. dsc starts at **v0.1.0** and
iterates independently:

```
v0.1.0 → v0.2.0 → v0.3.0 → …
```

deka will pin a dsc binary (or wasm artifact) by *this* version. The two
numbers are allowed — expected — to diverge.

## Policy

[Semantic Versioning 2.0](https://semver.org/) with one constraint: **v1.0.0
is a milestone we choose explicitly**, not something `major` should drift into
while we are still in `0.x`.

- **PATCH** (`0.1.0` → `0.1.1`): bug fixes, performance, CI, docs that do not
  change compiler output.
- **MINOR** (`0.1.x` → `0.2.0`): new commands, language/emit changes, ABI
  additions, or breaking changes that are not yet 1.0. This is the usual step.
- **MAJOR** (`0.x.y` → `1.0.0`): reserved for a stable 1.0.

The source of truth is `[workspace.package] version` in the root `Cargo.toml`.
Every crate uses `version.workspace = true`. Do not hand-edit crate files, and
do not set a crate to deka's version.

`scripts/dsc-version.sh` prints that version. Wasm `manifest.json` reads it
the same way.

## Bumping

```sh
./scripts/bump-version.sh minor     # 0.1.0 → 0.2.0
./scripts/bump-version.sh patch     # 0.1.0 → 0.1.1
./scripts/bump-version.sh 0.2.0     # explicit
./scripts/bump-version.sh --print
./scripts/bump-version.sh --dry-run minor
```

That updates `[workspace.package]`, any crate that still inlines a version,
and `Cargo.lock`. It refuses to go backwards, reuse a version that already
has a `v*` tag, or land on `0.42.x` (deka's line when the crates were
copied). It does not commit or tag.

Do **not** push the bump to `main`. Open a PR, merge with a merge commit, then
tag from `main`:

```sh
git checkout main
git pull origin main
scripts/dsc-version.sh              # must print the version you are about to tag
git tag -a "v$(scripts/dsc-version.sh)" -m "dsc v$(scripts/dsc-version.sh)"
git push origin "v$(scripts/dsc-version.sh)"
```
