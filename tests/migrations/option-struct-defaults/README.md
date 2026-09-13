# Omitted Option fields (dsc#180)

The checker now accepts omitted Option-typed fields in object and named struct
literals. An absent property reads as erased None. Required fields retain their
existing diagnostics. No JSON codec or Option representation changes are needed.

`testsuite.patch` applies at the `dekaruntime/testsuite` repository root, against
**corpus-v0.50.3**, archive SHA-256
`4bcf95bbf6c81b4c851bc1af95fb0cfd0e07f136a4e7e8c456d94ecea8da3205`.
It migrates `interfaces/interface_optional_omitted_in_literal_fail` from a
checker rejection to a runtime acceptance assertion, retaining the fixture ID.
Its `.code` changes from the old failed-source snapshot to the passing exit code
`0`; the assertion verifies `go({})` matches None and returns `/`.

The patch also includes all four new fixtures from
`tests/fixtures/option_struct_defaults/`; do not copy those twice. They cover
empty/default literals, partial arguments, nested interfaces and named structs,
typed JSON's tagged None envelope, and the unchanged required-field diagnostic.
No skip or expected-failure entries are added or changed.

Following the #172/#177 owner migration pattern, this compiler PR stays draft
until the owner corpus migration is released and a reviewed tag/checksum pin
update passes the native gate. This lane opens only the compiler PR; it does not
merge, tag, publish, or open another repository's PR.

Verification uses separate `.cache/testsuite-corpus-option-struct-defaults/`
and unchanged `.cache/testsuite-corpus/` trees. The tour needs no migration;
its unchanged commit is `e66d57409dd4f6967b3612f95867b00d03e40e25`.

## Validation

Native runs use the checksummed pinned deka runtime **0.50.0** (Darwin x64)
and this worktree's release dsc build. The unchanged pin's sole failure is
`interfaces/interface_optional_omitted_in_literal_fail`: it now runs successfully
instead of reporting the old `expected argument type` diagnostic.

| Gate | Passed | Failed | Skipped | Total |
| --- | ---: | ---: | ---: | ---: |
| Full Rust workspace (including doc tests) | 829 | 0 | 0 | 829 |
| Unchanged pinned tour | 90 | 0 | 0 | 90 |
| Unchanged corpus-v0.50.3 | 779 | 1 | 133 | 913 |
| Corpus with prepared owner patch | 784 | 0 | 133 | 917 |

The final exact-output assertions and four local Hats fixtures also pass in
`cargo test --locked -p deka_emit option_struct_defaults`. The UI JavaScript
test, wasm32 compiler/LSP checks, and `cargo clippy --locked --all-features`
pass (existing compiler/clippy warnings remain). `git apply --check` succeeds
against a clean copy of the pinned corpus; applying the patch reproduces the
verified migrated tree exactly.
