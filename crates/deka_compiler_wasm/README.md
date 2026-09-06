# Deka browser compiler WASM

`deka_compiler_wasm` is the internal crate implementing the versioned Deka
browser compiler ABI. The neutral `deka_compiler_*` exports are the only public
browser contract.

## ABI v2

- `deka_compiler_alloc(size)` and `deka_compiler_free(ptr, size)` own all input
  and result buffers.
- `deka_compiler_compile(source, filename, options)` accepts UTF-8 byte ranges.
  `options` is a JSON object: `{"mode": "deka", "moduleBase": "..."}`.
  `mode` is `auto` or `deka` (required); `moduleBase` is optional. When set,
  bare import specifiers are rewritten to `<moduleBase>/<spec>.mjs`.
  Input filenames must end in `.ds` or `.dsx`. PHPX modes, filenames, and
  ABI aliases are intentionally unsupported.
- The result is a pointer to `{ ptr: u32, len: u32 }`, followed by UTF-8 JSON:
  `{"abi_version":2,"ok":...,"output":{"code":...},"diagnostics":[...],"metadata":...}`.
  Diagnostics contain `severity`, `code`, `message`, `filename`, and
  1-based `start_line`, `start_column`, `end_line`, and `end_column` fields
  suitable for Monaco.
- `deka_compiler_metadata()` returns the compiler name, package version, and
  source commit without compiling.

## Reproducible artifact contract

Run `scripts/build-deka-compiler-wasm.sh [output-dir]` from a clean
checkout. It refuses dirty source trees, uses the pinned Rust toolchain,
`cargo --locked`, a clean incremental setting, and embeds the exact full Git
commit through `DEKA_SOURCE_COMMIT`. It writes:

- `deka_compiler.wasm`
- `deka_compiler.wasm.sha256`
- `deka_compiler.wasm.metadata.json`

The website must pin the full `source_commit` and expected `sha256`, download
the matching WASM and metadata from the release origin, verify that the file
hash equals both values, and reject the artifact unless the metadata schema is
`1`, the target is `wasm32-unknown-unknown`, and the embedded ABI metadata
matches the manifest. The manifest records the Cargo lock hash and complete
Rust compiler identity needed to reproduce the bytes. No artifact is committed
or published by this crate.

Run `scripts/test-deka-compiler-wasm.sh` to exercise native response
fixtures, build the release WASM, and instantiate it with the browser
`WebAssembly` API for the same success/diagnostic fixtures.
