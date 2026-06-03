# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.16.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.15.0...bougie-v0.16.0) (2026-06-03)


### Features

* **services:** attach to combined log stream on `bougie up` ([#300](https://github.com/cresset-tools/bougie/issues/300)) ([8b90051](https://github.com/cresset-tools/bougie/commit/8b90051146a05ff099c8c84dcaa91c6229dd2723))


### Bug Fixes

* **composer:** warn instead of erroring on stale composer.lock ([#304](https://github.com/cresset-tools/bougie/issues/304)) ([0a9bfcf](https://github.com/cresset-tools/bougie/commit/0a9bfcff4a1efa72a5111001d18d8f81c43fbf87))
* **fetch:** build the step bar with its draw target to stop a stranded frame ([#303](https://github.com/cresset-tools/bougie/issues/303)) ([bc97eb8](https://github.com/cresset-tools/bougie/commit/bc97eb8e9de5ec818e4cc13e93927fec35330ed0))
* **recipe:** pin bougie on PATH for check scripts ([#301](https://github.com/cresset-tools/bougie/issues/301)) ([addbd4e](https://github.com/cresset-tools/bougie/commit/addbd4e75c2ad17c9a2b4a62207b3404d2bad3bd))

## [0.15.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.14.0...bougie-v0.15.0) (2026-06-03)


### Features

* **daemon:** graceful shutdown + opensearch jdk runtime dep ([#297](https://github.com/cresset-tools/bougie/issues/297)) ([a21a051](https://github.com/cresset-tools/bougie/commit/a21a051384a8a63d8291ae3e951bfd27cfb03cb7))
* **installer:** count progress for baseline extension install ([#296](https://github.com/cresset-tools/bougie/issues/296)) ([045fee6](https://github.com/cresset-tools/bougie/commit/045fee64207b98ed5bc8d441e3b892c6adfa6d42))
* SIGQUIT activity dump + shared resolver metadata cache ([#295](https://github.com/cresset-tools/bougie/issues/295)) ([fc7c3cb](https://github.com/cresset-tools/bougie/commit/fc7c3cb211b935afc0fb57f790d839f4cd4a51ae))


### Bug Fixes

* don't orphan rabbitmq when bougied gets a foreground Ctrl-C ([#299](https://github.com/cresset-tools/bougie/issues/299)) ([385f4e5](https://github.com/cresset-tools/bougie/commit/385f4e5db63dce9afdb8c1adb9a35dd9c180d5bf))

## [0.14.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.13.0...bougie-v0.14.0) (2026-06-02)


### Features

* **server:** default web (php-fpm) memory_limit to 1G ([#294](https://github.com/cresset-tools/bougie/issues/294)) ([cf61811](https://github.com/cresset-tools/bougie/commit/cf61811981ee3c10e68f4ec98e529837c4ba37ca))
* **shim:** default CLI php to memory_limit=-1 (FPM unchanged) ([#292](https://github.com/cresset-tools/bougie/issues/292)) ([94a04b5](https://github.com/cresset-tools/bougie/commit/94a04b55183a16e52e03c970ece95cebe822f69b))


### Bug Fixes

* **babysit:** don't tear down a healthy service when the sidecar exits benignly ([#291](https://github.com/cresset-tools/bougie/issues/291)) ([cd5bbf9](https://github.com/cresset-tools/bougie/commit/cd5bbf9d4ff80878d08f7b86043ecf2857da0d63))

## [0.13.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.12.0...bougie-v0.13.0) (2026-06-02)


### Features

* **babysit:** co-locate a service's helper daemon via --sidecar (epmd, macOS-correct) ([#285](https://github.com/cresset-tools/bougie/issues/285)) ([30486ad](https://github.com/cresset-tools/bougie/commit/30486ad063e0bdc6e80109a3fe0b5a8f271ca7b0))


### Bug Fixes

* **daemon:** anchor bougied cwd so provisioner probes survive a deleted launch dir ([#289](https://github.com/cresset-tools/bougie/issues/289)) ([a30a83c](https://github.com/cresset-tools/bougie/commit/a30a83c318cf6a0e6dc2ef560f50897abea699c5))
* **daemon:** derive mariadb passwords so they survive down/purge/re-provision ([#287](https://github.com/cresset-tools/bougie/issues/287)) ([4eee91f](https://github.com/cresset-tools/bougie/commit/4eee91fec043795eb58121f479ee9991c50b002d))
* **daemon:** derive rabbitmq passwords too (stable across re-provision) ([#290](https://github.com/cresset-tools/bougie/issues/290)) ([98d1025](https://github.com/cresset-tools/bougie/commit/98d10250c82994b8d9a7d61caf86b9aa359f12a8))
* **server:** keep generated/ classmap entries fresh instead of dangling ([#288](https://github.com/cresset-tools/bougie/issues/288)) ([dcbc3be](https://github.com/cresset-tools/bougie/commit/dcbc3be5cd9aa69316a07eb4b81ad3958365a188))

## [0.12.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.11.0...bougie-v0.12.0) (2026-06-02)


### ⚠ BREAKING CHANGES

* **composer:** `bougie composer {fetch,uninstall,list,find,pin,dir,upgrade}` are removed. Pin the composer version via bougie.toml instead.

### Features

* **composer:** trim surface to native ops; make Composer a default project-aware tool ([#277](https://github.com/cresset-tools/bougie/issues/277)) ([970c751](https://github.com/cresset-tools/bougie/commit/970c7512956d94ffd51aacd988df10e2ae5406e6))
* **daemon:** cgroup-v2 supervision backend — reap daemonized escapees (e.g. epmd) ([#283](https://github.com/cresset-tools/bougie/issues/283)) ([535e198](https://github.com/cresset-tools/bougie/commit/535e198a7e708a39a1db207963e3ae3c313fc226))
* **init:** add --name flag and a new &lt;directory&gt; command ([#284](https://github.com/cresset-tools/bougie/issues/284)) ([e184eb9](https://github.com/cresset-tools/bougie/commit/e184eb9360ee0a580226ae53beaf1d36b6a21861))
* **self-update:** only update a binary bougie's installer placed ([#279](https://github.com/cresset-tools/bougie/issues/279)) ([f929f26](https://github.com/cresset-tools/bougie/commit/f929f2676d2fcd6cabf13e45ef57baa0bb490cbe))


### Bug Fixes

* **babysit:** SIGKILL the service via PR_SET_PDEATHSIG if the babysit dies abnormally ([#282](https://github.com/cresset-tools/bougie/issues/282)) ([df48680](https://github.com/cresset-tools/bougie/commit/df486804303a1ae8e852ae56aa76852e020ead75))
* **backend:** clearer error for an unsupported host target (musl/Alpine) ([#274](https://github.com/cresset-tools/bougie/issues/274)) ([9c789cb](https://github.com/cresset-tools/bougie/commit/9c789cb66ce42bb513dd91a26356100f87e3db46))
* **release:** bump-minor-pre-major so pre-1.0 breaking changes stay pre-major ([#280](https://github.com/cresset-tools/bougie/issues/280)) ([fa36828](https://github.com/cresset-tools/bougie/commit/fa36828b127a1d6c7be418841cd152a25797cd6a))
* **server:** serve on-disk static assets before the front-controller rewrite ([#281](https://github.com/cresset-tools/bougie/issues/281)) ([43e4cd5](https://github.com/cresset-tools/bougie/commit/43e4cd585977003d3250a4008b5410e30572db8a))

## [0.11.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.10.1...bougie-v0.11.0) (2026-06-01)


### Features

* **daemon:** forward the extracting phase to the CLI's mirrored download bar ([#272](https://github.com/cresset-tools/bougie/issues/272)) ([ecbb147](https://github.com/cresset-tools/bougie/commit/ecbb147c076dc6435da6e1f573a356d947f9756e))

## [0.10.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.10.0...bougie-v0.10.1) (2026-06-01)


### Bug Fixes

* **daemon,recipe:** restore services on daemon restart; pin recipe bougie to current exe ([#267](https://github.com/cresset-tools/bougie/issues/267)) ([0988460](https://github.com/cresset-tools/bougie/commit/098846081dcb76b8c59b90b963e14a41df3b6d69))
* **daemon:** plan the whole tool tree up front so the download bar total is accurate ([#271](https://github.com/cresset-tools/bougie/issues/271)) ([819c1bc](https://github.com/cresset-tools/bougie/commit/819c1bcca456fed1b01d03d992b6e7f5004ad9e4))
* **fetch:** add stall timeout, retries with backoff, and extraction progress ([#270](https://github.com/cresset-tools/bougie/issues/270)) ([8245965](https://github.com/cresset-tools/bougie/commit/824596539b43551d0d3659a2d503af75b623c442))
* **resolver:** honor root composer.json wildcard `replace` ([#269](https://github.com/cresset-tools/bougie/issues/269)) ([e14720d](https://github.com/cresset-tools/bougie/commit/e14720d6fa17113b1849d600209d5652c8f900f3))

## [0.10.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.9.0...bougie-v0.10.0) (2026-05-31)


### Features

* **init:** treat --starter as a base URL, append /starter.json ([#265](https://github.com/cresset-tools/bougie/issues/265)) ([6a7b958](https://github.com/cresset-tools/bougie/commit/6a7b958e3686c38d136d2fc9a2c6ea80e5f6d005))

## [0.9.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.8.3...bougie-v0.9.0) (2026-05-31)


### Features

* **init:** bougie init --starter &lt;url|alias&gt; + --start ([#263](https://github.com/cresset-tools/bougie/issues/263)) ([bfb5bcd](https://github.com/cresset-tools/bougie/commit/bfb5bcdce03ca77f461d14a5afd2c636404fb94f))

## [0.8.3](https://github.com/cresset-tools/bougie/compare/bougie-v0.8.2...bougie-v0.8.3) (2026-05-31)


### Bug Fixes

* **release:** allow-dirty = ["ci"] so dist accepts the hand-edited trigger ([#261](https://github.com/cresset-tools/bougie/issues/261)) ([88be819](https://github.com/cresset-tools/bougie/commit/88be81911d47b4ef3a7f86b75a0ca08264ec1850))

## [0.8.2](https://github.com/cresset-tools/bougie/compare/bougie-v0.8.1...bougie-v0.8.2) (2026-05-31)


### Bug Fixes

* **release:** let release-please own the whole release; dist only uploads ([#260](https://github.com/cresset-tools/bougie/issues/260)) ([d404a11](https://github.com/cresset-tools/bougie/commit/d404a116e113e574a7d137f0e171d8057d6665b0))
* **release:** suppress the candidate PR on release-merge runs ([#258](https://github.com/cresset-tools/bougie/issues/258)) ([ed025ce](https://github.com/cresset-tools/bougie/commit/ed025ce8be971a25fba71b361f8f03af5d3fe8d9))

## [0.8.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.8.0...bougie-v0.8.1) (2026-05-31)


### Bug Fixes

* **release:** move release-tag push into its own isolated job ([#253](https://github.com/cresset-tools/bougie/issues/253)) ([1570fc1](https://github.com/cresset-tools/bougie/commit/1570fc1e8d041cf82f305ee2818ff177371b08c1))

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
