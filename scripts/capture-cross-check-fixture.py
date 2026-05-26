#!/usr/bin/env python3
"""Capture a cross-check fixture for a project.

Walks Packagist v2 metadata breadth-first from the project's
composer.json, then runs Composer against the captured metadata (via a
local HTTP server) to produce the expected lockfile. Saves everything
to `crates/bougie/tests/fixtures/cross-check/<slug>/`.

Usage:
    python scripts/capture-cross-check-fixture.py <slug> <composer.json> [composer-phar]

Examples:
    python scripts/capture-cross-check-fixture.py monolog \
        /tmp/monolog/composer.json

    python scripts/capture-cross-check-fixture.py carbon \
        /tmp/carbon/composer.json /usr/local/bin/composer

Requirements:
    - Network access to repo.packagist.org
    - PHP + Composer (phar or binary) on PATH (or passed as third arg)
    - zstd CLI tool for compression

Output:
    crates/bougie/tests/fixtures/cross-check/<slug>/
      composer.json
      packagist-index.json.zst
      expected.json
"""

from __future__ import annotations

import http.server
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_BASE = REPO_ROOT / "crates" / "bougie" / "tests" / "fixtures" / "cross-check"
PACKAGIST_BASE = "https://repo.packagist.org"


def is_platform(name: str) -> bool:
    return (
        name == "php"
        or name == "hhvm"
        or name == "composer"
        or name == "composer-plugin-api"
        or name == "composer-runtime-api"
        or name.startswith("ext-")
        or name.startswith("lib-")
    )


def fetch(url: str, attempts: int = 4) -> bytes | None:
    last_err: Exception | None = None
    for i in range(attempts):
        try:
            req = urllib.request.Request(url)
            req.add_header("User-Agent", "bougie-cross-check-capture/1.0")
            with urllib.request.urlopen(req, timeout=20) as resp:
                return resp.read()
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            last_err = e
        except Exception as e:
            last_err = e
        time.sleep(0.5 * (2**i))
    raise RuntimeError(f"giving up on {url}: {last_err}")


def parse_deps_from_doc(doc_bytes: bytes) -> set[str]:
    doc = json.loads(doc_bytes)
    names: set[str] = set()
    for entries in doc.get("packages", {}).values():
        for entry in entries:
            for field in ("require", "require-dev", "replace", "provide"):
                value = entry.get(field)
                if not isinstance(value, dict):
                    continue
                for n in value:
                    if not is_platform(n):
                        names.add(n)
    return names


# ---- Phase 1: BFS-walk Packagist metadata ----


def capture_metadata(
    root_composer: dict,
) -> dict[str, dict]:
    """Return {name: p2_doc} for the full transitive closure."""
    seeds: set[str] = set()
    for field in ("require", "require-dev"):
        for n in (root_composer.get(field) or {}):
            if not is_platform(n):
                seeds.add(n)

    print(f"root seeds: {len(seeds)}")

    visited: set[str] = set()
    queue: list[str] = sorted(seeds)
    index: dict[str, dict] = {}

    with ThreadPoolExecutor(max_workers=16) as pool:
        while queue:
            batch = queue
            queue = []
            futures = {}
            for name in batch:
                if name in visited:
                    continue
                visited.add(name)
                fut = pool.submit(
                    fetch, f"{PACKAGIST_BASE}/p2/{name}.json"
                )
                futures[fut] = name

            for fut in as_completed(futures):
                name = futures[fut]
                try:
                    body = fut.result()
                except Exception as e:
                    print(f"  ERROR {name}: {e}", file=sys.stderr)
                    continue
                if body is None:
                    print(f"  404 {name}", file=sys.stderr)
                    continue
                doc = json.loads(body)
                index[name] = doc
                try:
                    for sub in parse_deps_from_doc(body):
                        if sub not in visited:
                            queue.append(sub)
                except Exception as e:
                    print(f"  parse {name}: {e}", file=sys.stderr)

            print(
                f"  wave done: visited={len(visited)} "
                f"captured={len(index)} pending={len(queue)}"
            )

    print(f"captured {len(index)} packages")
    return index


# ---- Phase 2: serve metadata via local HTTP server ----


class PackagistHandler(http.server.BaseHTTPRequestHandler):
    index: dict[str, bytes] = {}

    def do_GET(self) -> None:
        if self.path == "/packages.json":
            body = json.dumps(
                {"metadata-url": "/p2/%package%.json"}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        # /p2/<vendor>/<name>.json
        if self.path.startswith("/p2/") and self.path.endswith(".json"):
            name = self.path[4:-5]  # strip /p2/ and .json
            body = self.index.get(name)
            if body is not None:
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return

        self.send_response(404)
        self.end_headers()

    def log_message(self, format: str, *args: object) -> None:
        pass  # suppress request logging


def start_local_server(
    index: dict[str, dict],
) -> tuple[http.server.HTTPServer, int]:
    PackagistHandler.index = {
        name: json.dumps(doc, separators=(",", ":")).encode()
        for name, doc in index.items()
    }

    server = http.server.HTTPServer(("127.0.0.1", 0), PackagistHandler)
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, port


# ---- Phase 3: run Composer against the local server ----


def run_composer(
    composer_json: dict,
    port: int,
    composer_bin: str,
) -> dict:
    """Run `composer update` against the local server; return the lockfile."""
    with tempfile.TemporaryDirectory() as tmpdir:
        # Write a modified composer.json pointing at our local server.
        modified = dict(composer_json)
        modified["repositories"] = [
            {"type": "composer", "url": f"http://127.0.0.1:{port}"},
            {"packagist.org": False},
        ]
        # Remove scripts/plugins — we don't test those.
        modified.pop("scripts", None)
        modified.pop("extra", None)

        cj_path = Path(tmpdir) / "composer.json"
        cj_path.write_text(json.dumps(modified, indent=2))

        # Set COMPOSER_HOME to an isolated temp dir so global config
        # doesn't interfere.
        composer_home = Path(tmpdir) / "composer-home"
        composer_home.mkdir()
        # Allow http:// connections to the local server.
        (composer_home / "config.json").write_text(
            json.dumps({"config": {"secure-http": False}})
        )

        env = dict(os.environ)
        env["COMPOSER_HOME"] = str(composer_home)

        cmd = [
            *shlex.split(composer_bin),
            "update",
            "--no-install",
            "--no-plugins",
            "--no-scripts",
            "--no-interaction",
            "--ignore-platform-reqs",
            f"--working-dir={tmpdir}",
        ]
        print(f"running: {' '.join(cmd)}")
        result = subprocess.run(
            cmd,
            env=env,
            capture_output=True,
            text=True,
            timeout=300,
        )
        if result.returncode != 0:
            print(f"STDOUT:\n{result.stdout}", file=sys.stderr)
            print(f"STDERR:\n{result.stderr}", file=sys.stderr)
            raise RuntimeError(
                f"composer update failed with exit code {result.returncode}"
            )

        lock_path = Path(tmpdir) / "composer.lock"
        if not lock_path.exists():
            raise RuntimeError("composer.lock was not created")

        return json.loads(lock_path.read_text())


def extract_expected(lock: dict) -> dict:
    """Extract the expected package set from a composer.lock."""
    packages = {}
    for p in lock.get("packages", []):
        packages[p["name"]] = p["version"]

    packages_dev = {}
    for p in lock.get("packages-dev", []):
        packages_dev[p["name"]] = p["version"]

    return {"packages": packages, "packages_dev": packages_dev}


# ---- Phase 4: write fixture files ----


def write_fixture(
    slug: str,
    composer_json: dict,
    index: dict[str, dict],
    expected: dict,
) -> Path:
    out_dir = FIXTURE_BASE / slug
    out_dir.mkdir(parents=True, exist_ok=True)

    # composer.json — original (without repo overrides).
    (out_dir / "composer.json").write_text(
        json.dumps(composer_json, indent=2) + "\n"
    )

    # expected.json
    (out_dir / "expected.json").write_text(
        json.dumps(expected, indent=2, sort_keys=True) + "\n"
    )

    # packagist-index.json.zst
    raw = json.dumps(index, separators=(",", ":")).encode()
    zst_path = out_dir / "packagist-index.json.zst"
    proc = subprocess.run(
        ["zstd", "-19", "--long", "-T0", "-f", "-o", str(zst_path), "-"],
        input=raw,
        check=True,
        capture_output=True,
    )
    if proc.stderr:
        sys.stderr.write(proc.stderr.decode())

    compressed = zst_path.stat().st_size
    print(
        f"wrote {slug}/packagist-index.json.zst: "
        f"{compressed / 1e6:.2f} MB "
        f"({100 * compressed / max(1, len(raw)):.1f}% of "
        f"{len(raw) / 1e6:.1f} MB raw)"
    )
    return out_dir


# ---- Main ----


def find_composer() -> str:
    for name in ("composer", "composer.phar"):
        path = shutil.which(name)
        if path:
            return path
    # Try via php
    for candidate in (
        Path.home() / ".composer" / "composer.phar",
        Path("/usr/local/bin/composer"),
        Path("/usr/bin/composer"),
    ):
        if candidate.is_file():
            return f"php {candidate}"
    return ""


def main() -> int:
    if len(sys.argv) < 3:
        print(
            "usage: capture-cross-check-fixture.py <slug> <composer.json> "
            "[composer-bin]",
            file=sys.stderr,
        )
        return 2

    slug = sys.argv[1]
    cj_path = Path(sys.argv[2])
    composer_bin = sys.argv[3] if len(sys.argv) > 3 else find_composer()
    if not composer_bin:
        print("composer not found on PATH; pass as third arg", file=sys.stderr)
        return 2

    if not cj_path.is_file():
        print(f"not found: {cj_path}", file=sys.stderr)
        return 2

    composer_json = json.loads(cj_path.read_text())
    print(f"fixture: {slug}")
    print(f"source: {cj_path}")
    print(f"composer: {composer_bin}")

    # Check for scripts/plugins — warn but continue.
    if composer_json.get("scripts"):
        print(
            "WARNING: composer.json has scripts — they will be ignored "
            "in the cross-check (bougie never runs scripts)",
            file=sys.stderr,
        )
    for pkg_name in list(composer_json.get("require", {})) + list(
        composer_json.get("require-dev", {})
    ):
        # Simple heuristic for plugin packages.
        if "plugin" in pkg_name.lower():
            print(
                f"WARNING: {pkg_name} looks like a plugin — the "
                "cross-check won't run it",
                file=sys.stderr,
            )

    # Phase 1: capture metadata.
    print("\n=== Phase 1: capturing Packagist metadata ===")
    index = capture_metadata(composer_json)
    if not index:
        print("no packages captured — nothing to do", file=sys.stderr)
        return 1

    # Phase 2: start local server.
    print("\n=== Phase 2: starting local Packagist server ===")
    server, port = start_local_server(index)
    print(f"listening on http://127.0.0.1:{port}")

    try:
        # Phase 3: run Composer against it.
        print("\n=== Phase 3: running Composer ===")
        lock = run_composer(composer_json, port, composer_bin)
        expected = extract_expected(lock)
        print(
            f"resolved: {len(expected['packages'])} prod + "
            f"{len(expected['packages_dev'])} dev"
        )
    finally:
        server.shutdown()

    # Phase 4: write fixture.
    print(f"\n=== Phase 4: writing fixture to {FIXTURE_BASE / slug} ===")
    out_dir = write_fixture(slug, composer_json, index, expected)
    print(f"\nDONE: {out_dir}")
    print(
        f"run: cargo test -p bougie --test composer_cross_check "
        f"--features cross-check-fixtures -- corpus::{slug}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
