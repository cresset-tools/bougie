# NODE_PLAN — Node.js toolchain support

`bougie node` mirroring `bougie php`: PHP projects routinely need node/npm for
frontend assets (Vite, Laravel Mix, Magento static-content deploy). bougie is
positioned to provision node because the PHP-toolchain machinery is the right
shape (resolve version → fetch verified blob → extract into install tree → put
on PATH).

## Key decisions

- **Official `nodejs.org/dist` only.** No node-build-standalone analog needed:
  official node binaries are already relocatable and statically bundle
  V8/OpenSSL/zlib (the reason php/python-build-standalone exist doesn't apply).
- **Accept the glibc 2.28 floor (Node 18+).** No `nodejs/unofficial-builds`
  musl/glibc-217 fallback. Most users run newer glibc; the minority can't run
  node and that's acceptable. Adding unofficial-builds later is purely additive.
- **musl is rejected up front** with a clear "official Node requires glibc"
  error (probe via `bougie-platform` `Env::Musl`), not a cryptic exec-time
  `GLIBC_2.28 not found`.
- **`.tar.gz` not `.tar.xz`.** Node ships both for every Unix target; `.tar.gz`
  decodes via `flate2` (already in the lockfile) and avoids a new
  xz/liblzma dependency.
- **No `Flavor` for node.** Separate install tree `node-installs/<version>/`
  rather than contorting node through the PHP `-<flavor>` suffix.
- **Standalone backend, not the PHP `Backend` trait.** That trait is
  PHP-shaped (extensions, index closures, `Flavor`). Node is just
  resolve→one-blob→extract; it reuses `BlobRef` + `fetch_blob` but nothing else.

## Slices (tasks)

1. **bougie-fetch**: refactor `extract_tar_zst` → generic `extract_tar<R: Read>`;
   add `ArchiveKind::TarGz` (flate2 `GzDecoder`). Node tarballs contain
   symlinks (`bin/npm` → `../lib/node_modules/...`), already handled by
   `entry.unpack`.
2. **bougie-paths**: `Paths::node_installs()` → `local/node-installs`;
   `node_install_dir(paths, version)` → `node-installs/<version>/`.
3. **bougie-backend `nodejs_org`**: fetch `dist/index.json`, model
   `NodeRelease {version, lts, files}`, resolve request
   (latest/lts/major/major.minor/exact) → concrete version. Map `Triple` →
   `{x64,arm64}`×`{linux,darwin}.tar.gz` / `win.zip`. Fetch+parse
   `SHASUMS256.txt`. Return `NodeRecipe { version, BlobRef }`.
4. **CLI**: `NodeCommand {install,uninstall,list,find,dir}` + `Command::Node`;
   `commands/node.rs`; dispatch.
5. **`bougie run` PATH overlay** (done): `commands/node::project_bin_dir`
   prepends the project's node `bin/` onto PATH in both the direct-exec and
   composer-script paths. Detection priority:
   1. `.nvmrc`/`.node-version` (precise version)
   2. `package.json engines.node` (best-effort range)
   3. bare `package.json` presence (= node used, no pin)
   4. **Composer node-build dependency** — `hyva-themes/*` (Magento + Hyvä)
      or `snowdog/frontools`, checked in `composer.json` require/require-dev
      then a raw substring scan of `composer.lock` for the transitive case.
      Catches Magento+Hyvä, which has *no* root `package.json` (the Tailwind
      build lives in `app/design/frontend/.../web/tailwind/`).
   Pure-PHP (and Hyvä-less Magento) projects keep an untouched PATH; a
   node-wanting project with nothing installed gets a one-line stderr hint
   and still runs. Picks the highest installed version matching the filter.

## Remaining follow-ups

- **`bougie.toml [node]` version pin** — add `NodeConfig { version }` to
  `bougie-config` (model + merge) and fold it in as a fourth detection
  signal (highest priority) in `detect_project_node`. Deferred to avoid the
  config-merge churn in this slice; the ecosystem-standard signals
  (`.nvmrc`/`engines.node`) cover the common case.
- **corepack passthrough** for pnpm/yarn (`packageManager` field) — node's
  own mechanism; just needs `corepack enable` wiring or documentation.
- **auto-install on `run`** — currently we hint rather than download node
  mid-run (node isn't needed by every command, unlike PHP). Revisit if the
  hint proves annoying.
- **general subdir `package.json` scan** — the composer allowlist
  (`NODE_BUILD_PACKAGE_PREFIXES`) covers Hyvä and known tooling, but a
  custom theme/module with its own `package.json` and no recognized
  composer marker is still missed. A bounded source-tree walk (exclude
  `vendor/`, `node_modules/`, `var/`, `generated/`, `pub/static/`;
  early-exit on first hit) would be the catch-all — deferred for the
  per-`run` filesystem cost. Extend the allowlist for now when new
  node-build composer packages show up.

Delete this file when shipped (repo convention).
