# Compiler-owned helper catalog

`deka_syntax::deka_catalog` owns the closed RFD 21 table, safety/arity model,
and JavaScript implementations, ported from deka's runtime_core at
`f16c6a1c9ae4f57fd614c47aff3e609f80084b6d` (deka#881).

`safe { deka.kind.method(args) }` is a DekaScript AST expression. It emits a
plain call and uses the catalog's argument and return types. Fallible helpers
require `unsafe { ... }`; the existing unsafe wrapper converts exceptions to
Result values. Safe helpers may also be called under unsafe. Bare helper calls,
unknown helpers, wrong arity, spread arguments, and escaping the namespace
through an alias/computed access are compile errors at their original source
locations. An exhaustive AST walk covers defaults, guards, unwrap alternatives,
bridge arguments, JSX interpolations, and build blocks. SWC checks calls inside
raw unsafe JavaScript, including template interpolations and escaped identifiers.
The pre-existing `deka.ui` host capability and `deka.panic` language item are
separate surfaces, as in the former pool gate; this catalog adds no host API.

Filesystem sources get their package identity from the nearest `deka.json`,
after canonicalizing the source path. Package-store boundaries prevent a
manifest-less dependency from inheriting its consumer's identity. Only `@deka/`
package names qualify. Trusted virtual loaders can supply `CompileOptions`'s
`package_name` or implement `ModuleLoader::package_name`; unknown virtual sources
are unprivileged. A virtual identity never overrides an existing disk source.
There is no environment variable that changes the catalog or grants access.

Compilation inserts a collision-free module-local binding:

```js
const __dsc_catalog = (function () {
  // CATALOG_HELPERS_JS: frozen helper object and frozen namespaces
})();
// compiled safe call
const length = (__dsc_catalog.bytes.len(b));
```

SWC source spans rebind calls without altering strings/comments. The binding is
not exported and never assigned to `globalThis` or a prototype. Each module
owns its helper object; no host import or runtime helper installation is needed.
Option constructors are read lazily (the only implementation adaptation from
deka), so helper initialization may precede the normal compiler prelude. That
prelude remains the owner of branded Option/Result values. Catalog use requests
enum prelude demand for both standalone output and detached module graphs.
Separate build-plan entries receive the same private helpers.

This changes the deka/dsc contract. Release it in lockstep with the later deka
PR removing the source scanner/staging rewrite, loader catalog gate, and helper
bootstrap/preamble installation. In particular, the old scanner's lowered bare
calls are intentionally no longer accepted. Dev uses the same dsc compiler path
and requires no exemption. This PR does not change deka or its release pins.

Context: dekaruntime/deka#881 and dekaruntime/rfd#61 (implementation ownership).
