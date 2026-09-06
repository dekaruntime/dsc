#!/usr/bin/env bash
# Print the lockstep dsc version from [workspace.package].
# Used by bump-version.sh, wasm builds, and (later) release.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

python3 - "$ROOT_DIR" <<'PY'
from __future__ import annotations

import re
import sys
from pathlib import Path

root = Path(sys.argv[1])


def first_version(text: str, section: str | None = None) -> str | None:
    if section:
        m = re.search(
            rf"(?ms)^\[{re.escape(section)}\]\s*(.*?)(?=^\[|\Z)",
            text,
        )
        if not m:
            return None
        text = m.group(1)
    m = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)
    return m.group(1) if m else None


ws = (root / "Cargo.toml").read_text()
version = first_version(ws, "workspace.package")
if not version:
    sys.stderr.write("could not read dsc version from [workspace.package] in Cargo.toml\n")
    sys.exit(1)
print(version)
PY
