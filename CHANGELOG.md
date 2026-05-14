# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
