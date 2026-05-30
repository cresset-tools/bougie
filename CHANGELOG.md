# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.7.0...bougie-v0.8.0) (2026-05-30)


### Features

* **installers:** native Composer install-plugin support (Magento, composer/installers, Laravel) ([#248](https://github.com/cresset-tools/bougie/issues/248)) ([ebdf9c3](https://github.com/cresset-tools/bougie/commit/ebdf9c31be080a26ce00196c5b4ceefb27b5599e))


### Bug Fixes

* **recipe:** Mage-OS one-command bring-up — detect mage-os, redis-over-socket, lock re-stamp ([#251](https://github.com/cresset-tools/bougie/issues/251)) ([4d29004](https://github.com/cresset-tools/bougie/commit/4d2900418697defb4bc17ecfcac98c498b31b784))
* **release:** push the release tag (draft Releases don't auto-tag) ([#249](https://github.com/cresset-tools/bougie/issues/249)) ([469ee13](https://github.com/cresset-tools/bougie/commit/469ee1373c5b22b3b35e5336dc907b14138a57a9))

## [0.7.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.6.4...bougie-v0.7.0) (2026-05-30)


### Features

* **cli:** uv-style --version with git sha, date, and target triple ([#243](https://github.com/cresset-tools/bougie/issues/243)) ([06293c3](https://github.com/cresset-tools/bougie/commit/06293c31da20d3b332a11de1687f7355eb771ed9))
* **self:** implement bougie self update ([#244](https://github.com/cresset-tools/bougie/issues/244)) ([3f0f200](https://github.com/cresset-tools/bougie/commit/3f0f200b5faee9a801f71e3183eb7148239cc889))
* **sync:** one-command install — create lock + vendor, learn PHP/exts from the lock ([#241](https://github.com/cresset-tools/bougie/issues/241)) ([7bd6a21](https://github.com/cresset-tools/bougie/commit/7bd6a21781fe26e422f709a87cc5bafe71458306))


### Bug Fixes

* **release:** jq key-access syntax for release-please-manifest ([#242](https://github.com/cresset-tools/bougie/issues/242)) ([7c0a5f4](https://github.com/cresset-tools/bougie/commit/7c0a5f408980e3cbc3962ff8208476c393c6863e))
* **release:** let release-please own the draft GitHub Release ([#245](https://github.com/cresset-tools/bougie/issues/245)) ([6b8ce18](https://github.com/cresset-tools/bougie/commit/6b8ce18395186d66963f96a2bb7e3056d2a9b0fe))

## [0.6.4](https://github.com/cresset-tools/bougie/compare/bougie-v0.6.3...bougie-v0.6.4) (2026-05-30)


### Bug Fixes

* **release:** let dist own the GitHub Release; release-please pushes tag only ([#238](https://github.com/cresset-tools/bougie/issues/238)) ([55ef8e5](https://github.com/cresset-tools/bougie/commit/55ef8e5d30a1d7e4bd2c5e79051a101c9973e135))
* resolve whole-project review findings ([#207](https://github.com/cresset-tools/bougie/issues/207)–[#231](https://github.com/cresset-tools/bougie/issues/231)) ([#234](https://github.com/cresset-tools/bougie/issues/234)) ([4f873e9](https://github.com/cresset-tools/bougie/commit/4f873e95dd96e62f4423b8cd0fe0f1a369038aab))

## [0.6.3](https://github.com/cresset-tools/bougie/compare/bougie-v0.6.2...bougie-v0.6.3) (2026-05-30)


### Bug Fixes

* **composer:** Mage-OS resolve fixes — caret ^0, self-replace, fetch retry ([#232](https://github.com/cresset-tools/bougie/issues/232)) ([96cef9e](https://github.com/cresset-tools/bougie/commit/96cef9ec36cb0d15d13f97a47e773f50244532e6))
* **release:** make release-please actually rewrite Cargo.toml ([#237](https://github.com/cresset-tools/bougie/issues/237)) ([ca40f63](https://github.com/cresset-tools/bougie/commit/ca40f63e432c7ddae1c491db0123fc8101ce1143))
* **release:** unblock musl + windows dist targets ([#233](https://github.com/cresset-tools/bougie/issues/233)) ([87705a9](https://github.com/cresset-tools/bougie/commit/87705a9ec70115f857bb84d9daa827dde5e58f15))

## [Unreleased]

## [0.4.0](https://github.com/cresset-tools/bougie/compare/v0.3.0...v0.4.0) - 2026-05-16

### Added

- *(up)* surface resolved tool dependencies in json-v1
- *(daemon)* warn on catalog vs requires_tools drift
- *(daemon)* recursively install requires_tools[] inner tools
- *(daemon)* walk closure[] when auto-fetching tool tarballs
- *(index)* add requires_tools to manifest schema
- *(services)* auto-detect supervised server docroot
- *(cli)* [**breaking**] promote `services up`/`services down` to top-level `up`/`down`
- *(services)* auto-fetch service tarballs on first `services up`
- *(services)* babysit shim for crash-safe process-group supervision
- *(services)* rabbitmq provisioner (Phase 10)
- *(services)* bougied self-restart on version mismatch (Phase 9)
- *(services)* bougie server as a managed service
- *(services)* opensearch provisioner with per-tenant index templates
- *(services)* mariadb provisioner + integration tests against real binary
- *(services)* log rotation + `bougie services logs [-f] [-n N]`
- *(services)* inject BOUGIE_SERVICE_* env into `bougie run`
- *(services)* redis provisioner + service.{up,down,status} IPC + CLI
- *(daemon)* supervisor state machine, sandbox compilation, tenants ledger
- *(services)* offline subcommands — add/remove/list/catalog
- *(services)* built-in catalog + [services] config schema
- *(services)* bougie services daemon {status,stop,version}
- *(daemon)* bougied entry point + JSON IPC dispatcher
- *(daemon)* vendor sandbox-run + wire bougied shim role and paths

### Fixed

- *(services)* re-sync rabbitmq password to broker after `bougie down` ([#31](https://github.com/cresset-tools/bougie/pull/31))
- *(babysit)* install SIGTERM handler before spawning the service
- *(opensearch)* pin OPENSEARCH_JAVA_HOME + detect early child exit in health probe
- *(services/mariadb)* pass --no-defaults to every mariadb invocation

### Other

- *(index)* drop RequiresTool.manifest_sha256
- [**breaking**] Debian-faithful baseline + --bare / --without flags
- *(services)* convert opensearch pre_start file I/O to tokio::fs
- *(services)* make opensearch provisioner async
- Merge remote-tracking branch 'origin/main' into feat/services-babysit
- Set default binary
- Merge pull request #14 from cresset-tools/feat/services-opensearch
- *(opensearch)* dump opensearch.log on services-up failure
- *(services/mariadb)* pick per-target tarball for the test fixture
- fix macOS-specific failures surfaced in PR #8 validation
- *(services)* end-to-end redis up/down/status integration tests
- *(services)* integration tests for bougied auto-spawn + IPC roundtrip
- [**breaking**] relicense from Apache-2.0 OR MIT to EUPL-1.2

## [0.3.0](https://github.com/cresset-tools/bougie/compare/v0.2.0...v0.3.0) - 2026-05-14

### Added

- *(composer)* add lts channel as a version request
- *(cli)* unify list commands with shared coloured renderer

### Other

- Merge pull request #7 from cresset-tools/worktree-unified-list

## [0.2.0](https://github.com/cresset-tools/bougie/compare/v0.1.0...v0.2.0) - 2026-05-14

### Added

- *(server)* colourise text-mode request log on TTY stderr

### Fixed

- *(ci)* switch release-plz to git_only mode

### Other

- *(release-plz)* authenticate via GitHub App instead of PR_BOT PAT
- refresh lockfile and bump sha2, md-5, anstream to latest majors
- prune stale per-project runtime dirs at startup + shutdown
- make `ext add`, `run --xdebug`, and server routing all work
- pre-download xdebug into the store without enabling it
- split conf.d into conf.d-debug; auto-activate xdebug on first request
- make project arg optional, auto-detect from composer.json
- warn on missing web root / missing index at add + run
- filter notify Access events in watcher to fix reload loop
- sudo-aware server.toml resolution
- canonicalize project path on `server add`
- phase 6 — control socket + live `server list`
- phase 5 — /etc/hosts auto-sync via manage_etc_hosts flag
- phase 4 — pool lifecycle (idle-out, LRU cap, watch reload)
- phase 3 — per-request xdebug pool routing
- phase 2 — FastCGI dispatch to per-project php-fpm pools
- phase 1 — foreground HTTP server with static-file dispatch
- phase 0 — config schema + add/remove/list helpers
- phased build order for bougie server
- one aggregate progress bar per orchestrator call
- strip storeName prefix on closure tarballs, link store/ peer
- walk manifest closure + fix conf.d prefix ordering
- Improve wording
- auto-install composer.json's require.ext-* (CLI.md §3.3 step 4(c))
- install and auto-enable a default extension set per CLI.md §3.5.1.1
- ext list: --only-available keeps the `installed` marker visible
- honor config.sort-packages when editing require maps
- ext add/remove: drop composer subprocess; do the work ourselves
- manifest LoadDirective + install_extension + conf.d fragment writer
- lockfile + composer.json IO and editing primitives
- byte-exact PHP json_encode + Locker::getContentHash port
- add unzip role so composer's ZipDownloader prefers our extractor
- php list: colorize output uv-style, honoring NO_COLOR and pagers
- release v0.1.0
- use PR_BOT PAT for release-plz so it can open PRs and fan out

## [0.1.0](https://github.com/cresset-tools/bougie/releases/tag/v0.1.0) - 2026-05-10

### Other

- use PR_BOT PAT for release-plz so it can open PRs and fan out
- add release-plz for tag + GitHub release automation
- drop the release-mode matrix axis
- run cargo test across {ubuntu, macos} × {debug, release}
- gate BOUGIE_TRUST_ROOT_PATH on a Cargo feature, not debug_assertions
- phase4 — pass `fetch_root` a verifier factory
- ext list: implement filter flags and fix status taxonomy
- php list: implement filter flags
- php install/uninstall: accept multiple targets
- document every flag and render errors uv-style
- pin PHP_BINARY so composer @php scripts find the right php
- composer list: show all channel versions, paged on a tty
- bougie-managed phars with project-shim integration
- build verifier lazily in fetch_root
- ext list: show installed (.so on disk) and available (index) extensions
- strip leading `install/` prefix when extracting tarballs
- locate project root from env, argv[0] path, or cwd ancestors
- make `--` optional, auto-sync, export BOUGIE_PROJECT_ROOT
- follow versioned section URLs from the snapshot root
- switch to lean section + fat manifest wire schema
- anchor relative manifest URLs at the actual section file path
- Sigstore Bundle verification against pinned signer identity
- structured variants with operation + url + hint context
- point repository at cresset-tools/bougie
- point default index URL at index.bougie.tools
- phase 8: remaining commands
- phase 7: bougie sync end-to-end
- phase 6: blob fetch + bougie php install/uninstall/list/find
- phase 5: resolver + locks
- phase 4: index protocol + signature verification
- phase 3: config + request grammar + bougie init
- phase 2: trivial commands + shim dispatch
- phase 1: split into lib + foundation modules
- rework help text + magenta clap color theme
- tighten subcommand help text
- initial scaffolding
