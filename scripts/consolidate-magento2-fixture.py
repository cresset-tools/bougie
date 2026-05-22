#!/usr/bin/env python3
"""Consolidate the per-package magento2 fixture tree into a single
zstd-compressed JSON file the bench can load in one read.

Input:  tests/fixtures/magento2/packagist/<vendor>/<name>.json (~2500
        small files, ~35 MB)
Output: tests/fixtures/magento2/packagist-index.json.zst (~3-5 MB)

Layout of the consolidated file (uncompressed):

    { "<vendor>/<name>": <whole-p2-doc>, ... }

The bench reads it once with `zstd::decode_all`, slices it into per-name
bodies, and serves each through one wiremock route. No on-disk IO
during the bench inner loop.

Run after `capture-magento2-fixture.py` + `trim-magento2-fixture.py`.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_DIR = (
    REPO_ROOT / "crates" / "bougie-composer-resolver" / "tests" / "fixtures" / "magento2"
)
PACKAGIST_DIR = FIXTURE_DIR / "packagist"
OUT = FIXTURE_DIR / "packagist-index.json.zst"


def main() -> int:
    index: dict[str, dict] = {}
    for vendor_dir in sorted(PACKAGIST_DIR.iterdir()):
        if not vendor_dir.is_dir():
            continue
        vendor = vendor_dir.name
        for f in sorted(vendor_dir.glob("*.json")):
            # Skip any ~dev variants if they still happen to be here.
            if f.name.endswith("~dev.json"):
                continue
            name = f"{vendor}/{f.stem}"
            index[name] = json.loads(f.read_text())

    raw = json.dumps(index, separators=(",", ":")).encode()
    print(f"consolidated {len(index)} packages → {len(raw) / 1e6:.1f} MB raw")

    # zstd -19 long mode squeezes JSON best. Re-runnable; we don't
    # check it into LFS or anything fancy.
    OUT.parent.mkdir(parents=True, exist_ok=True)
    proc = subprocess.run(
        ["zstd", "-19", "--long", "-T0", "-f", "-o", str(OUT), "-"],
        input=raw,
        check=True,
        capture_output=True,
    )
    if proc.stderr:
        sys.stderr.write(proc.stderr.decode())
    compressed = OUT.stat().st_size
    print(
        f"wrote {OUT.name}: {compressed / 1e6:.2f} MB"
        f" ({100 * compressed / max(1, len(raw)):.1f}% of raw)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
