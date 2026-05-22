#!/usr/bin/env python3
"""Capture the magento2 closure for the resolver benchmark.

Walks Packagist v2 metadata breadth-first starting from the root
`composer.json` next to this script's fixture dir, snapshotting every
`/p2/<name>.json` (and `~dev.json` when present) into
`crates/bougie-composer-resolver/tests/fixtures/magento2/packagist/`.

The capture is the slow, network-bound, ratelimit-sensitive part of
`PR 0` in `RESOLVER_PERF_PLAN.md`. The bench itself loads these files
off disk and serves them via wiremock — no network during the bench.
Re-run this script only when the fixture needs refreshing.
"""

from __future__ import annotations

import json
import re
import sys
import time
import urllib.request
import urllib.error
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_DIR = REPO_ROOT / "crates" / "bougie-composer-resolver" / "tests" / "fixtures" / "magento2"
PACKAGIST_DIR = FIXTURE_DIR / "packagist"

PACKAGIST_BASE = "https://repo.packagist.org"

# Platform / virtual packages we never fetch.
def is_platform(name: str) -> bool:
    return (
        name == "php"
        or name == "hhvm"
        or name == "composer-plugin-api"
        or name == "composer-runtime-api"
        or name.startswith("ext-")
        or name.startswith("lib-")
    )


def safe_filename(name: str) -> str:
    # Packagist names are `vendor/name`; the existing in-process test
    # server (`update/tests.rs:mount_p2`) serves them at
    # `/p2/<vendor>/<name>.json`, so we keep the directory shape.
    return name


def fetch(url: str, attempts: int = 4) -> bytes | None:
    """Fetch a URL with backoff. Returns None on 404."""
    last_err: Exception | None = None
    for i in range(attempts):
        try:
            with urllib.request.urlopen(url, timeout=20) as resp:
                return resp.read()
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            last_err = e
        except Exception as e:  # noqa: BLE001
            last_err = e
        time.sleep(0.5 * (2 ** i))
    raise RuntimeError(f"giving up on {url}: {last_err}")


def fetch_metadata(name: str, *, dev: bool) -> bytes | None:
    suffix = "~dev" if dev else ""
    return fetch(f"{PACKAGIST_BASE}/p2/{name}{suffix}.json")


def parse_requires(doc_bytes: bytes) -> set[str]:
    """Return every package name appearing in any version's `require`
    + `require-dev` + `replace` + `provide` of the doc."""
    doc = json.loads(doc_bytes)
    names: set[str] = set()
    for entries in doc.get("packages", {}).values():
        for entry in entries:
            for field in ("require", "require-dev", "replace", "provide"):
                value = entry.get(field)
                # Packagist v2 "minified" format uses the string
                # `"__unset"` as a sentinel meaning "this field was
                # cleared on an older version relative to the newest";
                # treat anything non-dict as absent.
                if not isinstance(value, dict):
                    continue
                for n in value.keys():
                    if not is_platform(n):
                        names.add(n)
    return names


def write_fixture(name: str, suffix: str, body: bytes) -> None:
    rel = PACKAGIST_DIR / f"{safe_filename(name)}{suffix}.json"
    rel.parent.mkdir(parents=True, exist_ok=True)
    # Re-serialize compactly so the on-disk fixture is small + stable.
    doc = json.loads(body)
    rel.write_text(json.dumps(doc, separators=(",", ":"), sort_keys=False))


def main() -> int:
    PACKAGIST_DIR.mkdir(parents=True, exist_ok=True)

    composer_json_path = FIXTURE_DIR / "composer.json"
    if not composer_json_path.exists():
        print(f"missing root composer.json at {composer_json_path}", file=sys.stderr)
        return 2

    root = json.loads(composer_json_path.read_text())
    seeds: set[str] = set()
    for field in ("require", "require-dev"):
        for n in (root.get(field) or {}).keys():
            if not is_platform(n):
                seeds.add(n)
    print(f"root seeds: {len(seeds)}")

    visited: set[str] = set()
    queue: list[str] = sorted(seeds)
    fetched = 0

    with ThreadPoolExecutor(max_workers=16) as pool:
        while queue:
            batch = queue
            queue = []
            futures = {}
            for name in batch:
                if name in visited:
                    continue
                visited.add(name)
                fut = pool.submit(fetch_metadata, name, dev=False)
                fut_dev = pool.submit(fetch_metadata, name, dev=True)
                futures[fut] = (name, "")
                futures[fut_dev] = (name, "~dev")

            for fut in as_completed(futures):
                name, suffix = futures[fut]
                try:
                    body = fut.result()
                except Exception as e:  # noqa: BLE001
                    print(f"  ERROR {name}{suffix}: {e}", file=sys.stderr)
                    continue
                if body is None:
                    # 404 — most commonly on ~dev. Skip silently.
                    continue
                write_fixture(name, suffix, body)
                fetched += 1
                if suffix == "":
                    # Walk transitive deps only from the stable doc;
                    # ~dev's branches don't add new packages — every
                    # branch sits under the same vendor/name.
                    try:
                        for sub in parse_requires(body):
                            if sub not in visited:
                                queue.append(sub)
                    except Exception as e:  # noqa: BLE001
                        print(f"  parse {name}: {e}", file=sys.stderr)

            print(
                f"wave done: visited={len(visited)} files={fetched} pending={len(queue)}"
            )

    print(f"\nDONE. visited={len(visited)} files written={fetched}")
    print(f"fixture dir: {PACKAGIST_DIR}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
