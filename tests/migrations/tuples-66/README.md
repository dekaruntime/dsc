# Native tuples (rfd#66)

`testsuite.patch` applies at the `dekaruntime/testsuite` repository root against
**corpus-v0.51.3**, archive SHA-256
`04c66bee0e299e182751dbe91ab30a65859bf8f89982b8d39a5a6afed691795c`.

The sole collision is `parser/destructuring_let_rejected`. Its source remains
rejected: the bare `[1, 2]` initializer is an array, and declaration destructuring
requires a tuple. The fixture moves from a parser diagnostic to a typechecker
diagnostic that teaches adding a tuple annotation. The patch changes only the
metadata title, stage, and expected diagnostic. No skips or expected failures
are added, and no passing fixture changes behavior.

The compiler PR remains draft pending an owner corpus release and reviewed
pin/checksum update. This lane opens one compiler PR and does not merge or
publish either repository. The original pinned corpus stays unchanged; the
migration is verified in `.cache/testsuite-corpus-tuples-66/`.

## Implemented disambiguation

A bracket literal is a tuple only when checked with an expected tuple type.
Annotations (including aliases), function parameters and returns, summon
signatures, and declared struct fields supply that context. Context reaches
nested tuple positions, `Some` payloads, and tuple elements of an annotated
array. Generic tuple parameters infer their type arguments positionally.

Bare literals remain homogeneous arrays. Destructuring alone does not turn
an array into a tuple: use `const pair: [number, string] = [1, "a"]` followed
by `const [n, s] = pair`, or `const [n, s]: [number, string] = [1, "a"]`.
Empty and singleton tuple types use `[]` and `[T]`. Existing parenthesized
match patterns retain their syntax.

A tuple literal erases to an ordinary JavaScript array, a declaration binding
to ordinary JS destructuring, and a proven tuple index to a bare subscript.
The fourth rfd#65 proof source is the tuple type's fixed length; range loops,
integer comparison pairs, and `has()` retain their existing array rules.
Nonliteral tuple indexes are rejected even when a binding holds a constant
integer. Position writes retain the existing mutable-binding checks.

## Validation

Native runs use the checksummed pinned deka **0.51.0** runtime (Darwin x64)
and this worktree's debug dsc build. The unchanged tour pin is
`ea5f38911652856bb21e3f61fca3cd3abe35889b` (99 sources; 98 native runner cases).

| Gate | Passed | Failed | Skipped | Total |
| --- | ---: | ---: | ---: | ---: |
| Full Rust workspace, including doc tests | 856 | 0 | 0 | 856 |
| Unchanged native tour | 98 | 0 | 0 | 98 |
| Unchanged corpus-v0.51.3 | 783 | 1 | 133 | 917 |
| Corpus with prepared owner patch | 784 | 0 | 133 | 917 |

Focused tests include parser/checker acceptance and diagnostics, nested tuples,
generic inference, exact JS output, real Node summon shims and whole-value null
guards, Option erasure, reflection/typed JSON, formatter parsing and idempotence,
and compiled module graphs with re-exports and captured destructured bindings.
`cargo clippy --locked --all-features` passes with warnings. The wasm32 compiler
and LSP checks also pass (`--no-default-features`).

All temporary files and build outputs stayed inside this worktree. Commands:

```sh
mkdir -p .work
export TMPDIR="$PWD/.work"
export CARGO_TARGET_DIR="$PWD/target"
cargo test --locked --workspace
cargo build --locked -p cli
DSC="$PWD/target/debug/dsc" bun .cache/tour/run.mjs
DEKA_NATIVE="$PWD/.cache/deka-runtime/deka" DEKA_DSC="$PWD/target/debug/dsc" \
  bun .cache/testsuite-corpus/run.mjs
# Separate copy with the staged owner patch applied:
DEKA_NATIVE="$PWD/.cache/deka-runtime/deka" DEKA_DSC="$PWD/target/debug/dsc" \
  bun .cache/testsuite-corpus-tuples-66/run.mjs
```

Run the workspace and native runners sequentially: the formatter corpus scan
also sees temporary DS files created and removed by the native runners.
