# Summon core (rfd#39, rfd#62)

```ds
opaque type Scene
summon {
  scene(): Exception<Scene, JsError>,
  total sceneAdd(scene: Scene, child: Scene): void,
  total selected(): Option<Scene>
} from "./vendor/scene.mjs"
```

The block uses the RFD's `name(parameters): Return` signatures. `total` prefixes
a signature; an optional `fn` is also accepted (`total fn name(...): Return`).
Every parameter and return needs an explicit type. Defaults belong to the JS
module, not the declaration. Opaque and summon declarations are top-level only.

Opaque types have nominal declaration identity, including across modules and
renamed imports. They have no constructor, visible fields, indexing, pattern
structure, or reflection surface. They may be held, passed, returned, and stored.
An opaque declaration erases completely; its foreign object is untouched.
Local receiver methods lower to free functions without modifying that object.
Export ordinary DS wrapper functions to expose a foreign operation to consumers.
Summoned bindings cannot be exported, captured, passed as values, or shadowed.

A summoned return requires `Exception<T, E>` unless explicitly marked `total`.
Existing asynchronous typing applies: `Promise<Exception<T, E>>` is fallible at
await; `total ...: Promise<T>` declares no checked rejection channel. Native
return/throw and promise rejection are preserved. There is no catch-and-wrap
adapter around the foreign function.

Every compilation parses the relative `.mjs` module with the compiler's SWC
parser. A missing/unparseable module, missing export, non-function export, or
incompatible arity is a hard diagnostic naming the module and export. Function
declarations, const function/arrow expressions, and local named export aliases
are supported. Default parameters and rest parameters determine the accepted
arity interval. Generators, mutable callable aliases, and unresolved re-exports
are rejected rather than assumed callable. Virtual graph loaders supply the same
module bytes; single-file hosts can use `CompileOptions.foreign_modules`.
No network fetch occurs. Tier-2 throw/totality analysis remains a later stage:
`total` is an explicit author claim, not a totality proof in this implementation.

Each successful non-Option, non-void return receives one nullish comparison.
Null/undefined violates the declared foreign contract and throws `BoundaryError`.
This is an unchecked bug outside the rfd#62 ledger, including on `total` calls.
Checked match and `.to_result()` rethrow this branded violation; ordinary
containment can catch it. A void return is exempt. Call arguments evaluate in the
authored frame, including arguments containing await, and the foreign call runs
once.

**Option seam (dsc#98):** inbound Option returns materialize the CURRENT tagged
Some/None representation: null and undefined become None; other values become
Some(value). Outbound Option parameters pass Some's payload and map None to
undefined. A shim must choose null instead if its API requires that absent form.
Do not remove this materialization until the paused Option-erasure work resumes.
The conversion applies at the declared function boundary, not recursively to
fields inside user-defined data structures; shims must provide their declared
DS field representations.

This stage supports the existing concrete DS types and opaque handles. `JsValue`
and checked narrowing are deferred, rather than exposing an untyped usable box.
The summon fetch/js_modules CLI, scaffolding, transitive module acquisition, and
tier-2 throw analysis are not part of this core change.

Paired conformance corpus: dekaruntime/testsuite#84. Its release owner must merge
and tag the corpus before this PR's compiler pin can be bumped and verified.
