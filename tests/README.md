# Language tests

This repo owns DekaScript language fixtures.

| Tree | What |
|---|---|
| `tests/tour/` | Lessons the website displays. Match by `id` in `manifest.json`, never by title. |
| `tests/testsuite/` | Hats folders (`.pass.ds` / `.fail.ds`). Compiler, fmt, and wasm consume these. Runtime execution stays on `deka`. |

Copied from `dekaruntime/deka` when dsc became the compiler. Do not keep a second copy in deka.
