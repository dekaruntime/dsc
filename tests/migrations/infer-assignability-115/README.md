# Infer is not universally assignable (dsc#115)

`Type::Infer` is no longer assignable to a concrete type. `Type::Var` still
unifies freely. Bare `unsafe { }` remains legal; using its Ok value as a
known type is the diagnostic, and the one-line fix is `unsafe<T> { ... }`.

These patches are the owner-side fixture updates, not a suppression list.

## Pins this patch is based on

| Owner | Pin | Checksum file |
| --- | --- | --- |
| Tour | `ea5f38911652856bb21e3f61fca3cd3abe35889b` | `scripts/tour-version` |
| Hats | `corpus-v0.52.0` | `scripts/testsuite-corpus-version` |

## Apply

```
# at dekaruntime/tour, tests/tour/
patch -p1 < path/to/dsc/tests/migrations/infer-assignability-115/tour.patch

# at dekaruntime/testsuite repository root
patch -p1 < path/to/dsc/tests/migrations/infer-assignability-115/testsuite.patch
```

Local verification against the fetched pins (after applying the same hunks
to `.cache/tour` and `.cache/testsuite-corpus`):

| Native owner runner | Unpatched pin | With prepared patch |
| --- | --- | --- |
| Tour | 94 passed, 4 failed / 98 | 98 passed, 0 failed / 98 |
| Hats | 778 passed, 6 failed, 133 skipped / 917 | 784 passed, 0 failed, 133 skipped / 917 |

Do not merge the compiler PR against the old pins. Release the reviewed
owner changes, then bump the tag/commit and SHA-256 in
`scripts/tour-version` and `scripts/testsuite-corpus-version`.

## Fallout (all class (a): genuine latent bugs)

Every failure was a bare `unsafe` Ok value used as a concrete `string` or
`number`. Annotating the success type makes the existing program check.

### Tour (4)

| Lesson | Hole | Fix |
| --- | --- | --- |
| `json-parse-and-result` | `JSON.parse` Ok is Infer; `v.answer` passed to `echo` | `unsafe<Parsed>` + `string(v.answer)` |
| `unsafe-blocks` | `21 + 21` Ok is Infer; passed to `echo` | `unsafe<number>` + `string(v)` |
| `deka-globals` | `Result<Infer, string>` assigned to `Result<number, string>` | `unsafe<number>` → `Result<number, JsError>` |
| `islands` | `renderToString` Ok is Infer; `r.html` passed to `echo` | `unsafe<Rendered>` + `e.message` on Err |

### Hats (6)

| Fixture | Hole | Fix |
| --- | --- | --- |
| `async/async_return_promise_unwraps` | `Promise.resolve` Ok is Infer; awaited as `number` | `unsafe<Promise<number>>` + match/await |
| `error_globals/deka_unsafe_catch` | thrown Ok/Err match is Infer; passed to `echo` | `unsafe<number>` + `string(...)` |
| `error_globals/json_parse` | parse Ok is Infer; `v.x` passed to `echo` | `unsafe<Parsed>` + `string(...)` |
| `error_globals/object_values` | `Object.values().join` Ok is Infer; unwrapped into `echo` | `unsafe<string>` |
| `unsafe/json_stringify_object` | stringify Ok is Infer; passed to `echo` | `unsafe<string>` + `e.message` on Err |
| `unsafe/unsafe_struct_literal_ok` | unannotated arrow Ok is Infer; called as `User` | `unsafe<fn() User>` |

No skip or expected-failure entries are added. Stdlib `echo` is already
annotated; it is not the 264-fixture cascade from the original probe.
