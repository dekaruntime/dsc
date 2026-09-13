# Imported interface payloads (dsc#184)

`api.ds`, `main.ds`, `deka.json`, `parse.mjs`, `log.mjs`, and
`api-original.txt` are copied from the deka#913 Phase 3 retry 2 probes at
`/Volumes/Projects/codex/summon-p3/probes/claims-cross-module`.

- `api.ds` infers a Result payload from a summoned Exception match.
- `api-original.txt` preserves the earlier explicit Result repro; `explicit.ds`
  is its executable fixture.
- `single.ds` is the single-module control.
- `main.stdout` is the exact successful output.

Run `cargo test --locked -p deka_compile --test cross_module_interfaces`.
The integration tests compile filesystem module graphs and execute their emitted
ES modules with Node and the real JS shims. Additional cases cover explicit
summoned Result annotations, omitted claims, thrown exceptions, type-only
exports/aliases/barrels, inferred forwarding functions, nested and recursive
interfaces, declaration-name collisions, methods/mutability, and invalid
fields/arguments/private imports.

The existing `export { Claims }` spelling exports an erased type binding.
A private interface carried by a public function signature needs no explicit
export to permit field access. Direct `export interface` syntax is tracked
separately in dsc#185; JSX attribute validation remains dsc#118's scope.
