# rfd#64 React runtime pairing

This lane implements the [adopted React emission amendment](https://github.com/dekaruntime/rfd/issues/64#issuecomment-5650232215).
Keep its dsc PR draft until the testsuite and tour owner changes, releases, and
reviewed checksum pin updates have landed in the pairing train.

## Reviewed bases

| Owner | Pin | SHA-256 |
| --- | --- | --- |
| testsuite | `corpus-v0.51.1` | `00c1ec3c14fa331d21bf300104c36fe9a23e38da95c75f8775c782e81f91a8f0` |
| tour | `e66d57409dd4f6967b3612f95867b00d03e40e25` | `6ab299f5b8c0fa3bf98beb376c709377e20e08e7dbcbd68683c95189f5ae008a` |

Apply `testsuite.patch` at the testsuite repository root (`corpus/` paths), and
`tour.patch` at the tour repository root. Both pins remain unchanged here.

## Every corpus migration

The unchanged corpus has one failing fixture:

- `components/jsx_element_not_number_fail`: its diagnostic still requires
  `found type Component`; JSX now has the opaque type `ReactNode`. The fixture
  still rejects assigning JSX to `number` and checks that exact type fact.

Five other negative fixtures need their JSX-return annotations changed from
`Component` to `ReactNode` so the original diagnostic remains their cause of
failure, without extra wrong-return diagnostics:

- `components/jsx_prop_missing_required_fail`
- `components/jsx_prop_type_mismatch_fail`
- `components/jsx_prop_unknown_fail`
- `components/interactive_component_without_client_directive`
- `hoisting/component_const_not_hoisted`

Authored `.code` mirrors are updated where present. The executable corpus pin
has no passing emitted-JS JSX snapshots to rewrite. No fixture status, stdout,
exit code, suppression list, or skip rule changes. Exact automatic-runtime
output and executable bundle coverage live in dsc's Rust tests.

## Every tour migration

Eight lessons change JSX result annotations to `ReactNode`:
`jsx-components`, `utility-classes`, `callbacks-with-function-types`, `islands`,
`jsx`, `props`, `suspense`, and `props-checked`. The last also changes the layout's
`children: Component` field to `children: ReactNode`. Lesson expectations and
executable statements stay intact. `Component<Props>` now names the function,
not the value it returns, so retaining those annotations would violate the
adopted amendment.

## Validation

| Native owner gate | Passed | Failed | Skipped | Total |
| --- | ---: | ---: | ---: | ---: |
| Unchanged corpus | 783 | 1 | 133 | 917 |
| Corpus plus prepared patch | 784 | 0 | 133 | 917 |
| Unchanged tour | 82 | 8 | 0 | 90 |
| Tour plus prepared patch | 90 | 0 | 0 | 90 |

Native runtime: pinned **deka 0.50.0**. Compiler: this lane's built dsc.
See [HANDOFF.md](HANDOFF.md) for the exact rerun and pairing sequence.
