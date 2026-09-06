# dsc

DekaScript compiler. Emits JavaScript. Does not run it.

The host is [`deka`](https://github.com/dekaruntime/deka). See [rfd#38](https://github.com/dekaruntime/rfd/issues/38).

## License

Copyright 2026 Sami Fouad. Licensed under the [Apache License, Version 2.0](LICENSE.md).

## CLI

Same shape as `deka`: a `Registry` in `crates/core`, commands that `register(&mut registry)`, `crates/cli` builds the list and dispatches.

```
dsc --help
```

Commands (`check`, `transpile`, `fmt`, `lsp`) are registered in `crates/cli/src/lib.rs`.

- `dsc` (no command) emits `app/`, `api/`, and `src/` to `dist/`.
- `dsc transpile <file-or-directory>` writes per-module `.js` (`--preserve`), or one graph with `--bundle`. `--treeshake` minifies.
- `dsc check --as-package <dir>` typechecks a local package through a scratch consumer + `.deka/links.json`. Registry install stays on `deka`.

## Development

Same as [`deka`](https://github.com/dekaruntime/deka): **PR → merge commit → tag**. Do not push to `main`. See [CONTRIBUTING.md](CONTRIBUTING.md).

## CI

Self-hosted only. No V8. sccache buckets are `dsc-sccache-{linux-x64,darwin-x64,darwin-arm64}`. `main` requires the **Rust tests** check.

## Versioning and release

Independent of deka. dsc starts at **v0.1.0** and iterates `v0.2.0`, `v0.3.0`, … — it does not inherit the runtime's `0.42` line.

Bump on a branch (`scripts/bump-version.sh minor`), merge the PR, then push an annotated `v*` tag from `main`. That tag is the release. See [VERSIONING.md](VERSIONING.md) and [PUBLISH.md](PUBLISH.md).

Browser artifacts: `scripts/build-wasm.sh` → `dsc.wasm` / `dsc_diagnostics.wasm` at `https://dsc-wasm.deka.gg`.
