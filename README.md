# dsc

<p align="left">
  <img src=".github/DS%20logo.png" alt="DekaScript logo" width="180">
</p>

DekaScript compiler. DekaScript goes in, JavaScript comes out.

**dsc** does not execute the emitted JavaScript. That task belongs to [deka](https://github.com/dekaruntime/deka).

## CLI

```
dsc --help
```

Commands (`check`, `transpile`, `fmt`, `lsp`, `summon`) are registered in `crates/cli/src/lib.rs`.

- `dsc` (no command) emits `app/`, `api/`, and `src/` to `dist/`.
- `dsc transpile <file-or-directory>` writes per-module `.js` (`--preserve`), or one graph with `--bundle`. `--treeshake` minifies. `--dev` emits `jsxDEV` with DS source locations.
- `dsc check --as-package <dir>` typechecks a local package through a scratch consumer + `.deka/links.json`. Registry install stays on `deka`.
- `dsc summon infer <module.mjs>` scaffolds a DRAFT summon block from a vendored module (`--out` writes a file). Review before committing; see [SUMMON.md](SUMMON.md).

## JSX and React

`.dsx` files emit React's automatic runtime: children belong in props, static
multiple children use `jsxs`, and `key` is argument three. Set `jsxRuntime` in
the nearest `deka.json` to the runtime module specifier your host resolves:

```json
{ "jsxRuntime": "@js/react/jsx-runtime" }
```

That is the default. A local module path can be used while package subpath
acquisition is being integrated. Preserved emission and `dsc bundle` use the
same lowering; the compiler does not embed a React vendor path or provide
`ui/jsx` / automatic `ui/reactive.live` adapters.

Hosts opt into development emission with `dsc transpile --dev`, `dsc check --dev`,
`dsc bundle --dev`, `CompileOptions.dev = true`, or `GraphCompileOptions.dev = true`;
WASM compile requests accept `"dev": true`.
The default remains production. Dev mode imports `jsxDEV` and emits
`jsxDEV(type, props, key, isStaticChildren, source, this)`, using `undefined`
when there is no key. `isStaticChildren` is true exactly where production uses
`jsxs`. The source object contains `fileName` (the supplied DS module path),
`lineNumber`, and `columnNumber` (one-based positions of the opening `<` in
`.dsx`, including Fragments).

`jsxDevRuntime` is an independent `deka.json` key, also available as
`jsx_dev_runtime` in Rust options and `jsxDevRuntime` in WASM requests. An
explicit option overrides the nearest manifest. If omitted, the dev specifier
uses the effective `jsxRuntime` base: replace its final slash-delimited segment
with `jsx-dev-runtime`. Thus `react/jsx-runtime` becomes
`react/jsx-dev-runtime`, `./runtime.mjs` becomes `./jsx-dev-runtime`, and a
specifier without a slash becomes `jsx-dev-runtime`. With no overrides the
default is `@js/react/jsx-dev-runtime`. Set `jsxDevRuntime` explicitly when a
custom host uses another naming convention. Production ignores this key.

```deka
interface Props { title: string }
fn Card(props: Props) ReactNode { return <p>{props.title}</p>; }
const View: Component<Props> = Card;
```

`Component<Props>` is a nominally diagnosed props-to-`ReactNode` function
contract and emits an ordinary JavaScript function with no wrapper. Props are
a checked interface or struct; JSX results use the opaque `ReactNode` type.
A bare `Component` annotation on a binding infers its props from the checked
function. Components evaluate dynamic expressions during render.

`useState`, `useRef`, `useEffect`, `createContext`, and `useContext` are
compiler-known (no import). Hook-ness is part of the function type
(`Hook<fn(...) T>`), the same Generic-wrapper shape as `Exception<>` on a
signature: any function that calls a hook-typed function is itself
hook-typed (fixed-point, transitive). Aliasing preserves the color; a
hook-typed function is not assignable to a plain `fn(...)` parameter.
Hook-typed functions are callable only from a Component (`ReactNode` return)
or another hook function. Hook calls must be straight-line: unconditional,
un-looped, and before any early return. `Setter<T>` accepts `T` or `fn(T) T`
and stays a plain value; `Ref<T>.current` is mutable. `useEffect` takes
`fn() Option<fn() void>` — `None` erases to `undefined` and `Some(cleanup)`
erases to the cleanup function, matching React's contract. The dependency
array is never written in DS; the compiler infers it from the effect's free
reactive bindings (useState values, props, hook results, including
`useContext` values) and omits stable identities (setters, refs). An
unclassifiable capture is a compile error.

`createContext<T>(default)` is a module-scope factory (not a hook) that
returns `Context<T>` with a total default — `useContext` of it is always
legal. `createContext<T>()` has no default: every visible render path of a
`useContext` use site must be wrapped in `<Ctx.Provider value={...}>`, or
the checker diagnoses the missing provider instead of falling back silently.
`<Ctx.Provider>` is the one allowed JSX member tag (RFD 8 otherwise).
Emission is the call as written plus
`import { useState } from` the React module implied by `jsxRuntime`
(`@js/react/jsx-runtime` → `@js/react`), keyed off resolved references so
aliases still import.

`data-deka-id` is injected on host JSX elements only when the compile graph
hydrates islands: any `client:*` directive, or interactive-component analysis
firing, anywhere in the graph. Plain components emit props byte-identical to
hand-written React. `ref` and event handlers pass through as ordinary props;
omitted optional props stay omitted. JSX spread attributes remain unsupported.
Event-handler hydration diagnostics still apply.

## WASM

A deka subset of features, this dsc compiler, and the deka lsp are compiled to WASM and made available open source via [web-ide-kit](https://github.com/dekaruntime/web-ide-kit).

## Development

Same as [`deka`](https://github.com/dekaruntime/deka): **PR → merge commit → tag**. Do not push to `main`. See [CONTRIBUTING.md](CONTRIBUTING.md).

## CI

PR CI runs on GitHub-hosted runners with a read-only token and no secrets. The required **Rust tests** check skips cargo work for documentation-only changes, but runs the full gate when source, tests, scripts, or CI inputs change. Release-only R2 credentials are never available to CI.

## Versioning

dsc versions in lockstep with deka, testsuite, and tour — one shared version
number across all four repos per RFD 59 (see [VERSIONING.md](VERSIONING.md)).

Bump on a branch (`scripts/bump-version.sh minor`), merge the PR, then push an annotated `v*` tag from `main`. That tag is the release. See [VERSIONING.md](VERSIONING.md) and [PUBLISH.md](PUBLISH.md).

Browser artifacts: `scripts/build-wasm.sh` → `dsc.wasm` / `dsc_diagnostics.wasm` at `https://dsc-wasm.deka.gg`.

## License

Copyright 2026 Sami Fouad < https://samifou.ad >. Licensed under the [Apache License, Version 2.0](LICENSE.md).
