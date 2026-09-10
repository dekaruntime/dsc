# Language tests

This repo owns DekaScript language fixtures.

| Tree | What |
|---|---|
| `tests/tour/` | Lessons the website displays. Match by `id` in `manifest.json`, never by title. |
| `dekaruntime/testsuite/corpus/` | Authoritative Hats folders (`.pass.ds` / `.fail.ds`), fetched via a checksummed CI pin. Runtime execution stays on `deka`. |

Copied from `dekaruntime/deka` when dsc became the compiler. Do not keep a second copy in deka.
