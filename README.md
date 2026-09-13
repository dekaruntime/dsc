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
- `dsc transpile <file-or-directory>` writes per-module `.js` (`--preserve`), or one graph with `--bundle`. `--treeshake` minifies.
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

```deka
interface Props { title: string }
fn Card(props: Props) ReactNode { return <p>{props.title}</p>; }
const View: Component<Props> = Card;
```

`Component<Props>` is a nominally diagnosed props-to-`ReactNode` function
contract and emits an ordinary JavaScript function with no wrapper. Props are
a checked interface or struct; JSX results use the opaque `ReactNode` type.
A bare `Component` annotation on a binding infers its props from the checked
function. Components evaluate dynamic expressions during render. State and
hooks require a future amendment.

The only injected prop is `data-deka-id`. `ref` and event handlers pass through
as ordinary props; omitted optional props stay omitted. JSX spread attributes
remain unsupported. Event-handler hydration diagnostics still apply.

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
