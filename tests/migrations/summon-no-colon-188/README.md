# dsc#188: colon-free summon returns (0.52.0)

Sami ruled return types colon-free everywhere. This is breaking and rides 0.52.0;
Ava owns landing/release timing after Fast Refresh. The diagnostic is permanent.
Archived spike evidence is exempt. No package versions or external pins change here.

## Package handoff to Ava

Apply each `<repo>.patch` at the corresponding `dekaruntime/<repo>` root using
`git apply --check /path/to/<repo>.patch` followed by `git apply` in its owner PR.
The exact original signature line numbers and reviewed main commits are below.
Parameter colons remain. Include package tests and io's README in the sweep.

| Repository | Base commit | Files and original lines |
| --- | --- | --- |
| io | `76f95b270e82dd9b1191cb0ec8418f82d152a656` | `README.md:17`; `index.ds:10` |
| bytes | `6320eaf70fd550e80c279b7e4a35f765148d52ec` | `index.ds:9`; `tests/roundtrip.ds:3` |
| time | `6faa16ff3da772287cfe62a0a002aee46d746ca4` | `index.ds:4`; `tests/roundtrip.ds:3` |
| crypto | `7aa058b90c1a1ddebbcde265dd972f4990777ad9` | `tests/roundtrip.ds:4` |
| cookies | `237775c0cf7c59bbef9d38c1997b6491cbb9a87d` | `index.ds:44,45,46,47,48,49,50`; `tests/roundtrip.ds:2,3,5` |
| http | `edaa847ccac707721a4e68c17874fbad57119f9f` | `tests/format.ds:1`; `tests/refused.ds:1`; `url.ds:24,25,26` |
| auth | `bab314a9625cc0d30f2ba53c907fe4f595ac9bfa` | `index.ds:39,40,41` |
| jwt | `de8e823637c8580f9c6c5178961fd19c3c41c900` | `index.ds:28,29,30,31,32`; `tests/roundtrip.ds:8,9,11` |

Crypto main currently uses `bridge` in `index.ds`; its only summon block is in
`tests/roundtrip.ds:4`. Reconcile any pending host-shim migration before landing.

## Pinned corpus

`testsuite.patch` applies at the `dekaruntime/testsuite` root against
`corpus-v0.51.4` (18 summon fixtures, including negative fixtures whose original
diagnostics must remain covered). No metadata, expected output, or ratchet changes.
Local verification uses `.cache/testsuite-corpus-no-colon-188`; the checksum-pinned
`.cache/testsuite-corpus` is retained unchanged. Ava applies the owner patch,
releases the corpus, then coordinates the tag/checksum bump before this train merges.
The old corpus deliberately fails under the new grammar; do not merge against it.

## Tour

`tests/tour/blocked/summon-intro/summon-intro.ds:7-8` already uses colon-free
signatures. Its syntax half unblocks with this PR. Sibling-module delivery remains
blocked: the runner only copies the lesson source, not `vendor.mjs`. Keep it out
of `manifest.json` until the tour owner fixes that harness. `tour.patch` updates
the blocker documentation against the commit in `scripts/tour-version`.

## Validation

Local macOS x64 validation uses the pinned deka 0.51.0 runtime, this branch's
`target/debug/dsc`, and a lane-local `TMPDIR=.cache/tmp` / `CARGO_TARGET_DIR=target`.

| Gate | Summary |
| --- | --- |
| `cargo test --locked --workspace` | 873 passed, 0 failed, 0 ignored (44 test/doc-test suites) |
| Native tour, migrated io | 98 passed, 0 failed / 98 |
| corpus-v0.51.4 + testsuite.patch, migrated io | 784 passed, 0 failed, 0 known failures, 133 skipped / 917 |
| Blocked summon-intro with sibling vendor.mjs supplied | Prints `6`, `Deka`, `could not decode`; remains excluded from the 98 lessons |
| Owner patches | All eight package patches, testsuite.patch, and tour.patch pass `git apply --check` |

Workspace coverage includes colon acceptance/rejection for named/anonymous/receiver
functions, interfaces, function types (also nested in summon parameters), and all
summon prefixes; exact diagnostic positions; infer golden drafts through both the
library and CLI; and formatter roundtrip/idempotence.

For native migration verification, seed `.cache/deka-packages/io` using
`scripts/ci-seed-tour-io.sh`, remove the return colon in its installed
`ds_modules/@deka/io/index.ds`, and recompute that scratch lock's `fsGraph.hash`
using deka-modules' SHA-256 algorithm: sorted relative file paths, NUL, file bytes,
newline per file (excluding build/dependency/cache trees and macOS metadata).
The locally migrated io hash was
`d7b4daec01ac1fd2e0738e9f100e9c6abea22dd37cf02f9197bb8c15c32da565`.
This is a local prepared-package check, not verification of a published new release.
No compiler integrity checks are disabled. Ava must publish/install the reviewed
package changes and regenerate locks for release validation.

Run the unchanged owner runners with:

```sh
export TMPDIR="$PWD/.cache/tmp"
DSC="$PWD/target/debug/dsc" bun .cache/tour/run.mjs
DEKA_NATIVE="$PWD/.cache/deka-runtime/deka" DEKA_DSC="$PWD/target/debug/dsc" \
  bun .cache/testsuite-corpus-no-colon-188/run.mjs
```

Unset `DEKA_NATIVE` for the tour command (its runner prefers that over `DSC`).
The original corpus and original released io intentionally fail: tour 34 passed /
64 failed, corpus 313 passed / 471 failed / 133 skipped. No skip/ratchet rules
were added to make the migrated runs pass. Logs are under `.cache/logs/`.
