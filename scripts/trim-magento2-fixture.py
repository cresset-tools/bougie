#!/usr/bin/env python3
"""Trim the captured magento2 fixture down to just the fields the
resolver reads during a pubgrub solve.

The full Packagist v2 doc carries dist/source/autoload/extra/time/etc.
The solver only needs: name, version, version_normalized, require,
require-dev, replace, provide. Dropping the rest shrinks the on-disk
fixture by ~20x without changing solve behavior.

Idempotent — safe to re-run.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
PACKAGIST_DIR = (
    REPO_ROOT
    / "crates"
    / "bougie-composer-resolver"
    / "tests"
    / "fixtures"
    / "magento2"
    / "packagist"
)

# Solver-relevant fields. See `crates/bougie-composer-resolver/src/
# update.rs` — the resolver only ever reads these.
KEEP_FIELDS = {
    "name",
    "version",
    "version_normalized",
    "require",
    "require-dev",
    "replace",
    "provide",
}


def trim_entry(entry: dict) -> dict:
    out: dict = {}
    for k in KEEP_FIELDS:
        if k in entry:
            v = entry[k]
            # Packagist's "minified" sentinel — preserve so the doc
            # still round-trips through Composer's diff applier.
            out[k] = v
    return out


def trim_file(path: Path) -> tuple[int, int]:
    src = path.read_bytes()
    before = len(src)
    doc = json.loads(src)
    pkgs = doc.get("packages", {})
    new_pkgs = {
        name: [trim_entry(e) for e in entries]
        for name, entries in pkgs.items()
    }
    # Keep `minified` so deserialization through
    # `bougie_composer::metadata::PackageMetadata` still sees the
    # Composer 2.0 minified-format marker.
    new_doc = {"packages": new_pkgs}
    if "minified" in doc:
        new_doc = {"minified": doc["minified"], "packages": new_pkgs}
    out = json.dumps(new_doc, separators=(",", ":")).encode()
    path.write_bytes(out)
    return before, len(out)


def main() -> int:
    total_before = 0
    total_after = 0
    n = 0
    for path in PACKAGIST_DIR.rglob("*.json"):
        before, after = trim_file(path)
        total_before += before
        total_after += after
        n += 1
    print(
        f"trimmed {n} files: {total_before / 1e6:.1f} MB → "
        f"{total_after / 1e6:.1f} MB"
        f" ({100 * total_after / max(1, total_before):.1f}%)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
