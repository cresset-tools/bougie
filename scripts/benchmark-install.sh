#!/usr/bin/env bash
# Benchmark a *warm-cache* `composer install` against `bougie sync` on the
# same project, and render the result as a PNG bar chart for the README.
#
# Fair-fight rules baked in:
#   - Both tools install the SAME composer.lock (generated once up front by
#     Composer itself, so the lock format is never the variable).
#   - Both caches are warmed before any timing: the global Composer download
#     cache and bougie's cache are fully populated, so neither run pays for
#     network I/O.
#   - bougie's PHP toolchain is installed up front, so `bougie sync` is timed
#     installing *packages*, never downloading a PHP runtime.
#   - Every timed run starts from a clean `vendor/` (hyperfine `--prepare`),
#     so we measure the extract + autoload-dump work, not a no-op.
#
# Tooling comes from the Nix devshell — run it that way:
#
#   nix develop --command scripts/benchmark-install.sh [output.png]
#
# Env overrides:
#   BOUGIE         path to the bougie binary (default: build target/release/bougie)
#   COMPOSER_PHAR  path to composer.phar  (default: .cache/composer-2.8.12.phar)
#   ITERATIONS     timed runs per tool    (default: 8)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$REPO_ROOT/target/bench/install-benchmark.png}"
# Resolve OUT to an absolute path now: the benchmark cd's into a temp project
# dir before rendering, so a relative output path would land there (and be
# cleaned up) instead of where the caller meant.
[[ "$OUT" = /* ]] || OUT="$PWD/$OUT"
ITERATIONS="${ITERATIONS:-8}"
COMPOSER_VERSION="2.8.12"
DEFAULT_PHAR="$REPO_ROOT/.cache/composer-$COMPOSER_VERSION.phar"
COMPOSER_PHAR="${COMPOSER_PHAR:-$DEFAULT_PHAR}"

# --- toolchain check ------------------------------------------------------
# hyperfine does the timing + warmup + stats, gnuplot draws the bars, jq
# reads hyperfine's JSON, php runs the composer phar. All four live in the
# devshell (flake.nix); curl is there too for the phar fetch.
missing=()
for t in hyperfine gnuplot jq php; do
    command -v "$t" >/dev/null 2>&1 || missing+=("$t")
done
if [[ ${#missing[@]} -gt 0 ]]; then
    echo "missing tools: ${missing[*]}" >&2
    echo "run inside the devshell: nix develop --command scripts/benchmark-install.sh" >&2
    exit 1
fi

# --- bougie binary --------------------------------------------------------
BOUGIE="${BOUGIE:-}"
if [[ -z "$BOUGIE" ]]; then
    echo "==> building bougie (release) ..." >&2
    cargo build --release -p bougie --quiet
    BOUGIE="$REPO_ROOT/target/release/bougie"
fi
if [[ ! -x "$BOUGIE" ]]; then
    echo "bougie binary not executable: $BOUGIE" >&2
    exit 1
fi

# --- composer phar --------------------------------------------------------
if [[ ! -f "$COMPOSER_PHAR" ]]; then
    echo "==> fetching composer $COMPOSER_VERSION ..." >&2
    mkdir -p "$(dirname "$COMPOSER_PHAR")"
    curl -sLo "$COMPOSER_PHAR" "https://getcomposer.org/download/$COMPOSER_VERSION/composer.phar"
fi

# --- isolated work dir ----------------------------------------------------
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
PROJECT="$WORK/project"
mkdir -p "$PROJECT"

# Composer gets its own HOME so the download cache we warm is the one we
# measure against (and we never touch the user's global Composer state).
# bougie deliberately uses the real BOUGIE_HOME so it reuses the PHP runtime
# and package cache already on the machine — that's the "warm" we want.
export COMPOSER_HOME="$WORK/composer-home"
export COMPOSER_NO_INTERACTION=1
mkdir -p "$COMPOSER_HOME"

# Mage-OS — the full Magento Open Source application tree (hundreds of
# packages). This is the realistic install bougie is built for, not a toy.
# The mage-os/product-community-edition metapackage pulls the entire module
# set from the Mage-OS composer mirror.
MAGEOS_VERSION="3.0.0"
cat > "$PROJECT/composer.json" <<JSON
{
    "name": "bougie/mageos-benchmark",
    "require": {
        "mage-os/product-community-edition": "$MAGEOS_VERSION"
    },
    "repositories": [
        { "type": "composer", "url": "https://repo.mage-os.org/" }
    ],
    "config": {
        "allow-plugins": {
            "mage-os/composer-dependency-version-audit-plugin": false,
            "*": true
        }
    }
}
JSON

cd "$PROJECT"

# --- setup: lock + warm both caches (all untimed) -------------------------
# 1. Resolve once with Composer to produce the shared composer.lock and warm
#    Composer's download cache.
# `--ignore-platform-reqs`: the Nix PHP lacks Mage-OS's full extension set
# (ext-intl/soap/xsl/gd/...), so let Composer resolve against the lock without
# a host platform check. bougie installs the *real* PHP + extensions itself
# during its warmup below, so its side of the fight is genuinely platform-sound.
echo "==> resolving lock + warming composer cache (this pulls the full Mage-OS tree) ..." >&2
php "$COMPOSER_PHAR" update --no-progress --no-audit --ignore-platform-reqs --quiet

# 2. Warm bougie: this installs the required PHP runtime (so it's NOT in the
#    timed runs) and populates bougie's package cache from the same lock.
echo "==> warming bougie (installs PHP + caches packages) ..." >&2
"$BOUGIE" sync >/dev/null

# Clean slate for the first timed run.
rm -rf vendor

# --- benchmark ------------------------------------------------------------
echo "==> benchmarking ($ITERATIONS runs each) ..." >&2
hyperfine \
    --warmup 3 \
    --runs "$ITERATIONS" \
    --prepare 'rm -rf vendor' \
    --command-name 'composer install' \
        "php $COMPOSER_PHAR install --no-progress --ignore-platform-reqs --quiet" \
    --command-name 'bougie sync' \
        "$BOUGIE sync --offline" \
    --export-json "$WORK/bench.json" \
    --export-markdown "$WORK/bench.md"

# --- chart data -----------------------------------------------------------
# gnuplot data rows: index<TAB>mean<TAB>label<TAB>colorint
#   composer -> amber (#f59e0b), bougie -> purple (#7c3aed, bougie brand).
jq -r '.results[] | "\(.command)\t\(.mean)"' "$WORK/bench.json" \
  | awk -F'\t' '{
        c = ($1 ~ /bougie/) ? 8141037 : 16095243;
        printf "%d\t%s\t%s\t%d\n", NR-1, $2, $1, c
    }' > "$WORK/bench.dat"

COUNT="$(wc -l < "$WORK/bench.dat" | tr -d ' ')"
CMEAN="$(jq -r '.results[] | select(.command|test("composer")) | .mean' "$WORK/bench.json")"
BMEAN="$(jq -r '.results[] | select(.command|test("bougie"))   | .mean' "$WORK/bench.json")"
# --- render (horizontal bars; bougie sits at the top row) -----------------
# Data rows are index 0 = composer, 1 = bougie, so plotting y = index puts
# bougie on top. gnuplot has no native horizontal histogram, so we draw the
# bars as boxxyerror rectangles spanning x = 0 .. mean.
XMAX="$(awk -v c="$CMEAN" -v b="$BMEAN" 'BEGIN { m = (c > b) ? c : b; printf "%.4f", m * 1.20 }')"
mkdir -p "$(dirname "$OUT")"
GP="$WORK/chart.gp"
cat > "$GP" <<'GPEOF'
set terminal pngcairo size 960,400 enhanced
set output OUT
set datafile separator "\t"
set style fill solid 0.9 border -1
set xlabel "seconds (mean) — lower is better"
set xrange [0:XMAX]
set yrange [-0.7:COUNT-0.3]
set grid xtics lc rgb '#dddddd'
set border 3
set tics nomirror
unset key
plot DATA using ($2/2.0):1:(0):2:($1-0.3):($1+0.3):4:ytic(3) with boxxyerror lc rgb variable, \
     DATA using 2:1:(sprintf("%.2f s", $2)) with labels left offset 1,0
GPEOF

gnuplot -e "DATA='$WORK/bench.dat'; OUT='$OUT'; COUNT=$COUNT; XMAX=$XMAX" "$GP"

echo >&2
cat "$WORK/bench.md" >&2
echo >&2
echo "==> chart written to $OUT" >&2
