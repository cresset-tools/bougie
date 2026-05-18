#!/usr/bin/env bash
# Generate per-fixture autoload expecteds by running Composer's own
# dump-autoload against minimal composer.json inputs.
#
# Re-run when:
#   - The pinned Composer version (REFERENCE_COMPOSER_VERSION in
#     crates/bougie-autoloader/src/lib.rs) bumps.
#   - You add a new fixture under crates/bougie-autoloader/tests/fixtures/.
#
# Each fixture dir holds:
#   input/      — committed source of truth (composer.json + PHP files
#                 for any path-referenced packages).
#   expected/   — generated output from this script. Overwritten on
#                 every run; do NOT hand-edit.
#
# Usage: scripts/generate-autoload-fixtures.sh [path/to/composer.phar]
#        (defaults to $REPO_ROOT/.cache/composer-2.8.12.phar if no arg given).
#
# The script's job is fixtures, not implementation; the actual bougie
# autoloader implementation lives in crates/bougie-autoloader.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURES_DIR="$REPO_ROOT/crates/bougie-autoloader/tests/fixtures"
DEFAULT_PHAR="$REPO_ROOT/.cache/composer-2.8.12.phar"

COMPOSER_PHAR="${1:-$DEFAULT_PHAR}"
if [[ ! -f "$COMPOSER_PHAR" ]]; then
    echo "missing composer phar at $COMPOSER_PHAR" >&2
    echo "fetch with: mkdir -p $REPO_ROOT/.cache && curl -sLo $DEFAULT_PHAR https://getcomposer.org/download/2.8.12/composer.phar" >&2
    exit 1
fi

# Script-level cleanup: each fixture appends its tempdir here and we
# remove them on exit. We can't use `trap ... RETURN` inside the
# fixture function — bash fires the RETURN trap when any sourced file
# also finishes, and we `source $input/bougie-flags`, which would
# wipe the tempdir mid-run.
CLEANUP_DIRS=()
cleanup_dirs() {
    for d in "${CLEANUP_DIRS[@]}"; do
        rm -rf "$d"
    done
}
trap cleanup_dirs EXIT

# Files we ship into expected/. autoload_*.php and installed.{json,php}
# are genuinely generated; ClassLoader.php / InstalledVersions.php /
# LICENSE are bundled verbatim from Composer's source but bougie ships
# its own pinned copies and emits them at dump time, so byte-diffing
# them in the harness catches drift between bougie's vendored bytes and
# what Composer 2.8.12 actually ships.
# `autoload.php` lives at vendor/autoload.php; everything else is
# under vendor/composer/. The harness mirrors this layout.
GENERATED_TOPLEVEL=(
    autoload.php
)
GENERATED_COMPOSER_DIR=(
    autoload_namespaces.php
    autoload_psr4.php
    autoload_classmap.php
    autoload_files.php
    autoload_real.php
    autoload_static.php
    installed.json
    installed.php
    ClassLoader.php
    InstalledVersions.php
    LICENSE
)

run_fixture() {
    local name="$1"
    local input="$FIXTURES_DIR/$name/input"
    local expected="$FIXTURES_DIR/$name/expected"

    if [[ ! -d "$input" ]]; then
        echo "skip $name: no input/ dir" >&2
        return
    fi

    # Run dump in a tempdir copy so we don't pollute the committed input.
    local work
    work="$(mktemp -d)"
    CLEANUP_DIRS+=("$work")
    cp -R "$input"/. "$work/"

    # Pull per-fixture flags out of input/bougie-flags (key=value
    # format, missing keys default to false). The Rust byte-
    # equivalence harness reads the same file, so the expected output
    # we generate here is asserted under the exact flag set the
    # harness will replay.
    local optimize=false classmap_authoritative=false no_dev=false apcu_autoloader=false
    local apcu_prefix="" autoloader_suffix=""
    if [[ -f "$input/bougie-flags" ]]; then
        # shellcheck disable=SC1090
        source "$input/bougie-flags"
    fi
    local extra=()
    [[ "$optimize" == "true" ]] && extra+=("--optimize-autoloader")
    [[ "$classmap_authoritative" == "true" ]] && extra+=("--classmap-authoritative")
    [[ "$no_dev" == "true" ]] && extra+=("--no-dev")
    [[ "$apcu_autoloader" == "true" ]] && extra+=("--apcu-autoloader")
    [[ -n "$apcu_prefix" ]] && extra+=("--apcu-autoloader-prefix=$apcu_prefix")
    # `autoloader_suffix` is a composer.json `config` setting, not a CLI
    # flag — the fixture's input/composer.json carries it directly and
    # composer reads it during `install`.

    (
        cd "$work"
        php "$COMPOSER_PHAR" install --no-progress --no-interaction --quiet "${extra[@]}"
    )

    # composer.lock is part of the input — bougie's dump_autoload reads
    # it for the package list and the content-hash (which Composer uses
    # to derive the ComposerAutoloaderInit<hash> class suffix). Path
    # repositories yield a deterministic lock since the dist reference
    # is sha1 of the committed package contents.
    cp "$work/composer.lock" "$input/composer.lock"

    # Materialize the installed vendor/<pkg>/ tree into the input so the
    # classmap scanner has source files to walk. bougie-autoloader is
    # not an installer — it scans the same vendor/ layout Composer's
    # installer step would produce, so the fixture commits that layout
    # alongside composer.{json,lock}. We rsync vendor/ minus the
    # composer/ subdir (which contains the *outputs* we're testing).
    rm -rf "$input/vendor"
    if [[ -d "$work/vendor" ]]; then
        mkdir -p "$input/vendor"
        for entry in "$work/vendor"/*; do
            [[ -e "$entry" ]] || continue
            base="$(basename "$entry")"
            [[ "$base" == "composer" ]] && continue
            [[ "$base" == "autoload.php" ]] && continue
            cp -R "$entry" "$input/vendor/$base"
        done
    fi

    rm -rf "$expected"
    mkdir -p "$expected/vendor/composer"
    for f in "${GENERATED_TOPLEVEL[@]}"; do
        src="$work/vendor/$f"
        [[ -f "$src" ]] && cp "$src" "$expected/vendor/$f"
    done
    for f in "${GENERATED_COMPOSER_DIR[@]}"; do
        src="$work/vendor/composer/$f"
        [[ -f "$src" ]] && cp "$src" "$expected/vendor/composer/$f"
    done

    echo "generated $name" >&2
}

# Iterate every fixture directory.
for dir in "$FIXTURES_DIR"/*/; do
    [[ -d "$dir" ]] || continue
    name="$(basename "$dir")"
    run_fixture "$name"
done

echo "done. fixtures committed at $FIXTURES_DIR" >&2
