# rfd#65 indexing migration train

The indexing gate deliberately breaks unguarded array reads and writes. This
repository's regressions are migrated with the gate. The authoritative tour and
Hats corpus are external checksum pins; these patches are the prepared owner
changes, not vendored fixture trees or a suppression list.

Ava coordinates these dependencies before the train lands:

1. Apply `testsuite.patch` at the `dekaruntime/testsuite` repository root, based
   on `corpus-v0.49.3`. Include this repository's `tests/fixtures/indexing/` group
   in the owner corpus PR. The patch also guards three negative fixtures so they
   continue testing their original type/mutability diagnostics.
2. Apply `tour.patch` at the `dekaruntime/tour` repository root, based on commit
   `94988b3461f6d44fbe96d46fa3b5321c4a608eeb`.
3. Release the reviewed owner changes, then update the tag/commit and SHA-256 in
   `scripts/testsuite-corpus-version` and `scripts/tour-version` in this train.
   Run the unchanged CI fetch scripts and both owner runners against those pins.

Do not merge the gate against the old pins. Local migration verification uses
separate `.cache/testsuite-corpus-indexing/` and `.cache/tour-indexing/` copies;
it does not change what the pinned CI fetches.

| Native owner runner | Original pin | With prepared patch |
| --- | --- | --- |
| Tour | 88 passed, 2 failed / 90 | 90 passed, 0 failed / 90 |
| Hats | 763 passed, 7 failed, 133 skipped / 903 | 770 passed, 0 failed, 133 skipped / 903 |

The seven passing Hats fixtures that need migration are:

- `data_types/array_filter_callback`
- `data_types/array_length_and_index`
- `data_types/array_map_callback`
- `data_types/array_mutation`
- `data_types/array_slice_subarray`
- `functions/hof_array_of_functions`
- `modules/collection-element-inferred-export`

The tour lesson IDs are `lists-objects-and-indexing` and `arrays`. All fixture
stdout expectations are unchanged. The 133 Hats skips are reported by the owner
runner; no expected-failure entries or skip rules were added here.
