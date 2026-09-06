# dsc

<p align="center">
  <img src=".github/DS%20logo.png" alt="DekaScript logo" width="180">
</p>

DekaScript compiler. DekaScript goes in, JavaScript comes out.

**dsc** does not execute the emitted JavaScript. That task belongs to [deka](https://github.com/dekaruntime/deka).

## CLI

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

`main` requires the **Rust tests** check. No pass, no merge.

## Versioning

Vercioning of dsc is independent of deka. There is no pinning in place. Deka is designed to be indifferent to which version of dsc is being used.

Bump on a branch (`scripts/bump-version.sh minor`), merge the PR, then push an annotated `v*` tag from `main`. That tag is the release. See [VERSIONING.md](VERSIONING.md) and [PUBLISH.md](PUBLISH.md).

Browser artifacts: `scripts/build-wasm.sh` → `dsc.wasm` / `dsc_diagnostics.wasm` at `https://dsc-wasm.deka.gg`.

## License

Copyright 2026 Sami Fouad < https://samifou.ad >. Licensed under the [Apache License, Version 2.0](LICENSE.md).
