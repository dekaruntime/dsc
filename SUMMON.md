# Summon core (rfd#39, rfd#62)

```ds
opaque type Scene
summon {
  scene() Exception<Scene, JsError>,
  total sceneAdd(scene: Scene, child: Scene) void,
  total selected() Option<Scene>
} from "./vendor/scene.mjs"
```

As of 0.52.0 (dsc#188), the block uses colon-free `name(parameters) Return`
signatures, like `fn`. A return-type colon is a permanent validation error:
``return types take no colon; remove `:` ``. `total` prefixes
a signature; an optional `fn` is also accepted (`total fn name(...) Return`).
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
parser when the host can read it. A missing/unparseable module, missing export,
non-function export, or incompatible arity is a hard diagnostic naming the
module and export. On platforms without module-read capability (wasm/browser),
verification is skipped with a note (`unverified: platform has no module access`)
rather than a hard error; host (native) builds remain the verifying authority.
Diagnostics that do not need module bytes (colon grammar, privacy, Exception
discipline, call-site arity) still fire. Function declarations, const
function/arrow expressions, and local named export aliases are supported.
Default parameters and rest parameters determine the accepted arity interval.
Generators, mutable callable aliases, and unresolved re-exports are rejected
rather than assumed callable. Virtual graph loaders supply the same module
bytes; single-file hosts can use `CompileOptions.foreign_modules`. The wasm
compiler accepts the same map as `foreignModules` in its JSON options.
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
The summon fetch/js_modules CLI and transitive module acquisition are not part
of this change.

**Draft scaffolder (`dsc summon infer`, rfd#39 authoring tier-3):** point the
tier-1 SWC walk at generation. The command takes a vendored `.mjs` path and
emits a `.d.ds`-style summon block to stdout or `--out`, marked
`DRAFT — review before committing`. Unknown parameter types become
opaque-candidate placeholders (never `JsValue`). Returns use the pessimistic
`Exception<T, JsError>` default. `total` is emitted only where visible analysis
of that module proves there are no throw sites: no uncaught `throw`, no
known-throwing intrinsics (`JSON.parse`, `decodeURI*`), and no calls into
unseen code. Same-module callees are followed. A contained `throw` inside
`try`/`catch` does not count. `total` is still a claim the author must own;
the draft never emits it silently. The summon fetch door remains a later stage.

Paired conformance corpus: dekaruntime/testsuite#84. Its release owner must merge
and tag the corpus before this PR's compiler pin can be bumped and verified.
