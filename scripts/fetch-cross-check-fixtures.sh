#!/usr/bin/env bash
# Fetch the cross-check corpus fixtures (frozen Packagist v2 metadata).
#
# These multi-MB `packagist-index.json.zst` blobs used to live in the
# repo via Git LFS. They are now hosted as a GitHub release asset and
# pulled on demand so a clone stays small. The corpus cross-check tests
# (`composer_cross_check.rs`, feature `cross-check-fixtures`) skip
# gracefully when the blobs are absent, so fetching is only needed when
# you actually want to run that suite:
#
#   scripts/fetch-cross-check-fixtures.sh
#   cargo test -p bougie --test composer_cross_check \
#       --features cross-check-fixtures
#
# Re-running is cheap: if every blob already matches the committed
# `fixtures.sha256` manifest, nothing is downloaded.
#
# Override the source with BOUGIE_FIXTURES_URL (e.g. a local mirror or a
# `file://` path) — handy for offline/CI caches.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURE_DIR="$REPO_ROOT/crates/bougie/tests/fixtures/cross-check"
MANIFEST="$FIXTURE_DIR/fixtures.sha256"

# Bump this tag whenever the fixtures are recaptured; keep it in step
# with the manifest so a stale cache can't masquerade as fresh.
TAG="cross-check-fixtures-v1"
ASSET="cross-check-fixtures-v1.tar"
DEFAULT_URL="https://github.com/cresset-tools/bougie/releases/download/$TAG/$ASSET"
URL="${BOUGIE_FIXTURES_URL:-$DEFAULT_URL}"

if [[ ! -f "$MANIFEST" ]]; then
  echo "error: manifest not found: $MANIFEST" >&2
  exit 1
fi

# Fast path: already present and verified → nothing to do.
if (cd "$FIXTURE_DIR" && sha256sum --quiet -c fixtures.sha256) >/dev/null 2>&1; then
  echo "cross-check fixtures already present and verified — nothing to fetch."
  exit 0
fi

echo "fetching cross-check fixtures from: $URL"
tmp_tar="$(mktemp)"
trap 'rm -f "$tmp_tar"' EXIT

case "$URL" in
  file://*) cp "${URL#file://}" "$tmp_tar" ;;
  *)        curl -fsSL "$URL" -o "$tmp_tar" ;;
esac

# Extract into the fixture tree. The tar carries the per-slug layout
# (`<slug>/packagist-index.json.zst`).
tar -xf "$tmp_tar" -C "$FIXTURE_DIR"

# Verify against the committed manifest — a mismatch means the release
# asset and the checked-out manifest have drifted.
if ! (cd "$FIXTURE_DIR" && sha256sum -c fixtures.sha256); then
  echo "error: fetched fixtures failed checksum verification" >&2
  echo "       the release asset may be out of sync with fixtures.sha256" >&2
  exit 1
fi

echo "cross-check fixtures fetched and verified."
