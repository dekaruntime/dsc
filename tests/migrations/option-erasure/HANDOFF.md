# Ava pairing handoff

Compiler lane: `/Volumes/Projects/codex/option-62/dsc`, branch
`feat/option-erasure-62`, base main `05340e872dc6605fc50cd164908d3a18236cd6be`
(indexing gate #172). Build output is isolated at this lane's `target/`.

1. Open the testsuite owner PR from the corpus-v0.50.1 base with
   `git apply --check testsuite.patch`, then `git apply testsuite.patch`.
   The patch includes both existing fixture migrations and all four additions.
2. Review and land that owner change, release a new corpus tag, and record the
   archive SHA-256. Ava coordinates these actions; this lane creates only the
   dsc draft PR and does not merge, tag, or publish owner changes.
3. Update `scripts/testsuite-corpus-version` in this compiler train to the
   reviewed release tag and checksum. The current compiler PR deliberately
   retains the original pin so the required pairing remains visible.
4. Fetch the new corpus with the unchanged fetch script and rerun the gates:

```sh
cd /Volumes/Projects/codex/option-62/dsc
export CARGO_TARGET_DIR="$PWD/target"
cargo test --locked --workspace --no-fail-fast
cargo build --locked --release -p cli
scripts/ci-fetch-testsuite-corpus.sh "$PWD/.cache/testsuite-corpus"
scripts/ci-fetch-tour.sh "$PWD/.cache/tour"
DEKA_NATIVE="$PWD/.cache/deka-runtime/deka" DEKA_DSC="$PWD/target/release/dsc" bun .cache/testsuite-corpus/run.mjs
DSC="$PWD/target/release/dsc" bun .cache/tour/run.mjs
```

Prepared migration validation used `.cache/testsuite-corpus-option/`, separate
from the checksum-pinned `.cache/testsuite-corpus/`. Patch application was
checked against a fresh extraction. The original pin fails exactly the two
nested-Option acceptance fixtures described in README.md; do not land against
that pin. The tour passes unchanged and needs no release or pin bump.

Reflection and JSON retain their public formats, as in Result PR #164:
checked `Option<T>.getType()` reports Option, and `.signature()` still carries
kind `option` and its inner descriptor. `.toJSON()` encodes
`{ "Option": { "case": "Some", "values": [v] } }` or
`{ "Option": { "case": "None" } }`. Parsing returns erased values within the
ordinary Result return value. A private decoder failure sentinel distinguishes
valid None from invalid JSON, including inside arrays, structs, and Result.
