# Language tests

This repo owns DekaScript language fixtures.

| Tree | What |
|---|---|
| `dekaruntime/tour` → `.cache/tour/` | Lessons the website displays, their manifest, and the native runner. Owned by `dekaruntime/tour`; fetched via a checksummed CI pin (`scripts/ci-fetch-tour.sh`). Match by `id` in `manifest.json`, never by title. Never vendor a copy here — one authority, consumed by both deka and dsc. |
| `dekaruntime/testsuite/corpus/` | Authoritative Hats folders (`.pass.ds` / `.fail.ds`), fetched via a checksummed CI pin. Runtime execution stays on `deka`. |

Both pins land under `.cache/`; the fetch scripts recreate them on demand:

```sh
scripts/ci-fetch-tour.sh
scripts/ci-fetch-testsuite-corpus.sh
```

## Running the tour natively with dsc

The tour runner (`.cache/tour/run.mjs`) compiles every lesson with the binary
from `DSC` (or `DEKA_NATIVE`, else `target/release/dsc`/`cli`). Lessons import
`@deka/io`, and dsc has no package manager, so seed the shared package cache
once with a deka CLI:

```sh
scripts/ci-seed-tour-io.sh /path/to/deka   # once
DSC=$PWD/target/release/dsc bun .cache/tour/run.mjs
```

## rfd#62 Exception core

The paired conformance changes live on `dekaruntime/testsuite` branch
`rfd62/exception-fixtures`. Run that checkout's `corpus/run.mjs` with
`DEKA_NATIVE` pointing to the pinned runtime and `DEKA_DSC` to this dsc build.
The owner must release the corpus and coordinate a tag/checksum pin bump:
the previous corpus deliberately expects `try` to be rejected and contains
the superseded lowercase-throw diagnostic. Do not tag a feature branch.
The updated tour is independently pinned by immutable commit and checksum.

This implementation preserves the existing Option and Result representations.
Async annotations retain the existing `Promise<Exception<T, E>>` spelling;
`await` exposes the checked exception channel. Shared `Ok` stays unqualified
through formatting so it can be target-typed. Qualified `Result.Ok` remains
in the Result channel. A bodyless `Ok` can target either return channel;
bodyless `Err` and `Throw` cannot cross channels.

Emitter coverage includes statement-level matches (const/let initializers,
returns and expression statements, including parentheses), explicit
conversion, native DS-to-DS delegation, typed catches and awaited calls.
Remaining work: statement lifting for bodyless matches nested within other
expression operands, such as `Ok(match ...)`. These currently produce an
explicit emission error instead of returning from an artificial IIFE.
