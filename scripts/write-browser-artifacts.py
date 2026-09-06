#!/usr/bin/env python3
"""Write website/testsuite-compatible compiler + diagnostics manifests."""
import json
import sys
from pathlib import Path


def main() -> None:
    (
        version,
        commit,
        cargo_lock,
        compiler_sha,
        compiler_size,
        diag_sha,
        diag_size,
        out,
    ) = sys.argv[1:]
    dest = Path(out)
    compiler = {
        "schemaVersion": 1,
        "compiler": {
            "name": "deka-browser-compiler",
            "version": version,
            "sourceCommit": commit,
            "abiVersion": 2,
        },
        "producer": {
            "schemaVersion": 1,
            "target": "wasm32-unknown-unknown",
            "cargoLockSha256": cargo_lock,
        },
        "artifact": {
            "file": "deka_compiler.wasm",
            "sha256": compiler_sha,
            "bytes": int(compiler_size),
        },
    }
    diagnostics = {
        "schemaVersion": 1,
        "diagnostics": {
            "name": "deka_diagnostics",
            "abiVersion": 1,
            "sourceCommit": commit,
        },
        "producer": {
            "schemaVersion": 1,
            "target": "wasm32-unknown-unknown",
            "cargoLockSha256": cargo_lock,
        },
        "artifact": {
            "file": "deka_diagnostics.wasm",
            "sha256": diag_sha,
            "bytes": int(diag_size),
        },
    }
    (dest / "deka-compiler-artifact.json").write_text(
        json.dumps(compiler, indent=2) + "\n"
    )
    (dest / "deka-diagnostics-artifact.json").write_text(
        json.dumps(diagnostics, indent=2) + "\n"
    )


if __name__ == "__main__":
    main()
