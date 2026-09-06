# DekaScript LSP

The DekaScript language server is launched with `deka lsp --stdio`. Its public
language identifier is `dekascript`, and it discovers, resolves, indexes, and
renames `.ds` files only.

## Build and run

```sh
cargo build --release -p dekascript_lsp
cargo run --release -p dekascript_lsp
```

## Browser diagnostics artifact

`scripts/build-deka-compiler-wasm.sh` emits the companion
`deka_diagnostics.wasm` artifact with its SHA-256 and metadata. Its stable ABI
is `deka_diagnostics_alloc`, `deka_diagnostics_analyze`,
`deka_diagnostics_free`, and `deka_diagnostics_metadata` (ABI version 1).
`deka_diagnostics_analyze` accepts UTF-8 source text and a UTF-8 URI/path and
returns JSON diagnostics with UTF-16 ranges. Only `.ds` contexts are accepted.

## Editor integration

Configure the editor to start the release `cli` binary with `lsp --stdio`, use
the `dekascript` language id, and associate the server only with `.ds` files.
The server accepts an optional initialization setting:

```json
{
  "dekascript": { "target": "server" }
}
```

`DEKA_MODULE_ROOT` can override the project root used for `php_modules`
resolution. Workspace roots come from `workspaceFolders` or `rootUri`, falling
back to the current directory.
