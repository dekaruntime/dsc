# rfd#62 Option erasure pairing

Option values now erase to the payload or `undefined`. The compiler rejects
`Option<void>` and `Option<Option<T>>`, including generic call instantiations.
This patch is an owner migration, not a vendored corpus or suppression list.
Ava owns the pairing dance; keep the dsc PR draft until the owner release and
checksum pin update have passed the native gates.

Base: `dekaruntime/testsuite` tag **corpus-v0.50.1**, archive SHA-256
`3b9cafea09e7e860ddfc00b25e46cce5c9bc7e89ad6cef8853643e2ea774c64b`.
Apply `testsuite.patch` at the testsuite repository root.

The two existing migrations retain the lexer/parser regression for adjacent
`>>` tokens in const annotations, parameters, and return types:

- `types/nested_generic_option`: `Option<Option<number>>` becomes
  `Option<Array<number>>`.
- `types/nested_generic_return`: the same replacement in both signature positions.

Their `ok` stdout expectations and pass status are unchanged. Separate new
`diagnostics/option_void`, `diagnostics/option_nested`, and
`diagnostics/option_bodyless` fixtures assert the forbidden forms. The added
`option_erasure/runtime` fixture verifies falsy presence, single evaluation,
Result-shaped payload objects, and valid/invalid JSON. All four new fixtures
are included in the patch; do not copy them a second time.

The pinned executable corpus contains no `__enum`/`__case` pokes to migrate.
Local compiler/catalog/summon tests did contain old shape assertions; those
now assert the erased runtime values directly.

| Native gate | Passed | Failed | Skipped | Total |
| --- | ---: | ---: | ---: | ---: |
| Unchanged corpus-v0.50.1 | 774 | 2 | 133 | 909 |
| Corpus with this patch | 780 | 0 | 133 | 913 |
| Unchanged tour pin | 90 | 0 | 0 | 90 |

No expected-failure entries, skip rules, exit-code expectations, or existing
stdout expectations were changed. No tour patch is needed. Its unchanged pin
is `e66d57409dd4f6967b3612f95867b00d03e40e25`, SHA-256
`6ab299f5b8c0fa3bf98beb376c709377e20e08e7dbcbd68683c95189f5ae008a`.
Native verification uses the pinned deka runtime **0.50.0** and this lane's
release dsc. See [HANDOFF.md](HANDOFF.md) for the landing sequence.
