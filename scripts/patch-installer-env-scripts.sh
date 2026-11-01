#!/bin/sh
# Keep the shell installer's PATH helper scripts in the install dir.
#
# Since dist 0.33.0, flat-layout installers (ours: binaries go straight into
# ~/.local/bin) write `env`/`env.fish` to ~/.config/bougie/ instead, and
# "migrate" the old ~/.local/bin/env{,.fish}: they delete those files and
# rewrite the `source` lines in the user's shell profiles. Those files are
# not ours alone. uv and every other dist-installed tool in ~/.local/bin
# writes the same ones, so the migration leaves e.g. uv's
# ~/.config/fish/conf.d/uv.env.fish sourcing a deleted file, and makes the
# user's PATH depend on ~/.config/bougie existing.
#
# We patch both flat-layout branches dist 0.33 added to `if false`, which
# restores the 0.32 behaviour exactly. The older flat check (receipt
# handling, indented deeper) is left alone. The match count is checked so a
# template change fails the release instead of shipping half a patch;
# re-check this after every cargo-dist-version bump.
#
# Usage: patch-installer-env-scripts.sh <bougie-installer.sh>
set -eu

installer="$1"
pattern='^    if \[ "\$_install_layout" = "flat" \]; then$'

count="$(grep -c "$pattern" "$installer" || true)"
if [ "$count" != 2 ]; then
    echo "patch-installer-env-scripts: expected 2 flat-layout env branches in $installer, found $count;" >&2
    echo "dist's installer template changed, so re-check this patch" >&2
    exit 1
fi

tmp="$installer.tmp"
sed "s/$pattern/    if false; then # bougie: keep env scripts in the install dir (scripts\/patch-installer-env-scripts.sh)/" \
    "$installer" > "$tmp"
sh -n "$tmp"
cat "$tmp" > "$installer"
rm -f "$tmp"
echo "patched $installer: env scripts stay in the install dir"
