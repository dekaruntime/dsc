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

## Ternary conformance (rfd#65, part 1)

`tests/fixtures/ternary/` contains five new Hats-format fixtures, following
corpus-v0.49.3's exception fixtures: `.pass.ds` / `.pass.dsx` / `.fail.ds`,
metadata JSON, and exact `.stdout` / exit `.code` for passing programs.
They are local feature regressions, not a vendored copy of the owner corpus.
`cargo test -p deka_emit ternary` checks every fixture and executes emitted
JavaScript with Node.js (also required by the UI runtime tests); JSX output
is syntax-checked because its imports require the host UI runtime. The fixtures
can be moved to the owner corpus in a coordinated corpus release.

## Indexing conformance (rfd#65, part 2)

`tests/fixtures/indexing/` covers range loops, integer comparison pairs, `has()`
reads/writes, single evaluation and laziness, and an unproven-index diagnostic.
`cargo test -p deka_emit indexing` checks the Hats metadata and executes the
passing fixtures with Node.js. The formatter roundtrip includes local fixtures.

Array reads and writes require a dominating proof. `has()` is a compiler builtin
with a `number` argument and `boolean` result; its integer and bounds predicate
emits inline. A proven subscript still emits the authored bare subscript.
`for i in 0..items.length` lowers to the existing C-style counting-loop AST;
that canonical counting form is recognized too. Comparison pairs require known
integerness (integer literals/bindings, counting-loop variables, or `has()`).
Facts currently identify local array bindings and index bindings/integer literals.
Rebind computed receivers or indexes to locals before guarding them. The
unproven-index diagnostic on a field-access or nested-index receiver teaches
that hoist; bare-identifier receivers keep the `has()` recipe.

Calls other than the builtin predicate, writes, suspension, and unsafe JS kill
facts conservatively, including possible alias mutation. Shadowing kills facts
for that name; closures and loop backedges cannot inherit stale proofs. The gate
also checks parameter defaults. String/bytes indexing and Option representations
are outside this array-indexing lane.

The required external owner patches and pin coordination are documented in
[migrations/indexing-98/README.md](migrations/indexing-98/README.md).

## Omitted Option fields (dsc#180)

`tests/fixtures/option_struct_defaults/` covers empty/default and partial object
literals, nested interfaces and named structs, and required-field rejection.
`cargo test -p deka_emit option_struct_defaults` checks the Hats fixtures with
Node.js and exact emitted output: omitted object properties stay absent, while
explicit `None` emits as `undefined`. Named structs retain their existing
optional-field emission convention. The nested fixture also checks that typed
JSON still encodes None in its tagged Option envelope.

The pinned corpus has one intentional old rejection to migrate. Its prepared
owner patch and landing dependency are in
[migrations/option-struct-defaults/README.md](migrations/option-struct-defaults/README.md).

## Imported interface payloads (dsc#184)

`crates/deka_compile/tests/fixtures/claims_cross_module/` preserves the
multi-module deka#913 JWT blocker and its single-module control.
`cargo test --locked -p deka_compile --test cross_module_interfaces` checks the
filesystem graphs and runs emitted ES modules with Node.js, including explicit
Result and inferred summoned Exception payloads. See the fixture README for
additional positive and negative coverage.

Interface member types travel with signatures as private metadata resolved in
the declaring module. `export { Claims }` additionally makes the erased type
binding importable, including through aliases and barrels; callers need not
import the type merely to inspect a returned value. Direct `export interface`
syntax is a separate gap (dsc#185). No corpus/tour pairing patch is required:
the unchanged corpus-v0.51.1 and pinned 90-lesson tour pass.

## Native tuples (rfd#66)

`cargo test --locked -p deka_syntax -p deka_emit -p deka_compile -p deka-fmt tuples_`
checks tuple construction/consumption, the intrinsic type-length index proof,
exact erased output, Node summon boundaries, module exports, and formatter
idempotence. Bracket literals acquire tuple types only from expected tuple
context; bare literals remain arrays. Declaration destructuring binds exactly
the tuple's arity.

The pinned corpus's former parser rejection for array destructuring needs a
metadata-only diagnostic migration. See
[migrations/tuples-66/README.md](migrations/tuples-66/README.md) for the owner
patch, rule details, and full validation summaries.
