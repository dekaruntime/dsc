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
