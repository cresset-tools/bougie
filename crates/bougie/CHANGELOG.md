# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.2](https://github.com/cresset-tools/bougie/compare/bougie-v0.6.1...bougie-v0.6.2) - 2026-05-29

### Added

- *(release)* ship dist binary pipeline + bougie.tools mirror ([#206](https://github.com/cresset-tools/bougie/pull/206))
- *(tool)* ship `bougie tool` (Phases 1–3) + incremental composer install ([#204](https://github.com/cresset-tools/bougie/pull/204))

## [0.6.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.6.0...bougie-v0.6.1) - 2026-05-28

### Added

- *(composer-install)* skip up-to-date packages by diffing against installed.json ([#194](https://github.com/cresset-tools/bougie/pull/194))
- *(cli)* implement bougie composer validate ([#189](https://github.com/cresset-tools/bougie/pull/189))
- *(composer-install)* platform requirement checks + --ignore-platform-reqs ([#187](https://github.com/cresset-tools/bougie/pull/187))
- *(composer-install)* generate vendor/bin proxy scripts ([#186](https://github.com/cresset-tools/bougie/pull/186))

### Fixed

- *(composer-resolver)* resolve all cross-check divergences ([#184](https://github.com/cresset-tools/bougie/pull/184))

### Other

- *(composer-resolver)* add Layer 4 cross-check harness + parity matrix ([#183](https://github.com/cresset-tools/bougie/pull/183))

## [0.6.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.5.1...bougie-v0.6.0) - 2026-05-26

### Added

- *(sync)* infer PHP version and required extensions ([#178](https://github.com/cresset-tools/bougie/pull/178))
- *(recipe)* print URL and admin creds after `bougie start` ([#171](https://github.com/cresset-tools/bougie/pull/171))
- *(daemon)* stream tarball download progress to the CLI ([#169](https://github.com/cresset-tools/bougie/pull/169))
- *(composer-install)* [**breaking**] warn instead of error on Composer plugins/scripts ([#160](https://github.com/cresset-tools/bougie/pull/160))

### Fixed

- *(release)* decouple bougie version + centralize workspace dep pins ([#180](https://github.com/cresset-tools/bougie/pull/180))
- *(run)* walk up to project root, not cwd ([#176](https://github.com/cresset-tools/bougie/pull/176))
- *(composer-install)* claim Composer/2 UA and reuse shared HTTP client for dist downloads ([#163](https://github.com/cresset-tools/bougie/pull/163))
- *(sync)* accept Composer wildcards in require.php ([#106](https://github.com/cresset-tools/bougie/pull/106)) ([#150](https://github.com/cresset-tools/bougie/pull/150))

### Other

- *(release)* track leaves in release-plz so it rewrites path-dep pins ([#182](https://github.com/cresset-tools/bougie/pull/182))
- mechanical sweep + missing_panics_doc ([#149](https://github.com/cresset-tools/bougie/pull/149)) ([#156](https://github.com/cresset-tools/bougie/pull/156))
- audit numeric casts (zero remaining) ([#154](https://github.com/cresset-tools/bougie/pull/154))
- cargo clippy --fix mechanical sweep ([#149](https://github.com/cresset-tools/bougie/pull/149)) ([#153](https://github.com/cresset-tools/bougie/pull/153))

## [0.5.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.5.0...bougie-v0.5.1) - 2026-05-22

### Fixed

- *(release)* re-pin `version = "..."` on every intra-workspace path
  dependency so release-plz's `cargo package` step can verify the
  manifest. Path-only deps were dropped in #143 on the premise that
  `publish = false` made the version pin inert, but `cargo package`'s
  manifest verify still rejects them, which broke every subsequent
  release-PR run ([#148](https://github.com/cresset-tools/bougie/pull/148)).
- *(release)* opt the newer workspace-split crates
  (`bougie-autoloader`, `bougie-babysit`, `bougie-composer-resolver`,
  `bougie-php-json`, `bougie-semver`) out of independent releases. They
  were missing `release = false` in `release-plz.toml`, which caused
  release-plz to mint per-crate `bougie-*-v0.5.0` tags alongside the
  main `bougie-v0.5.0` tag.

## [0.5.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.4.0...bougie-v0.5.0) - 2026-05-22

### Added

- *(composer-resolver)* solve-phase progress spinner + tracing logs ([#137](https://github.com/cresset-tools/bougie/pull/137))
- *(cli)* composer install falls back to resolve when composer.lock is missing ([#132](https://github.com/cresset-tools/bougie/pull/132))
- *(composer-resolver)* support composer.json `repositories` field (composer-type) ([#130](https://github.com/cresset-tools/bougie/pull/130))
- *(composer-resolver)* prefer-stable candidate ordering ([#126](https://github.com/cresset-tools/bougie/pull/126))
- *(composer-resolver)* write composer.lock from bougie composer update ([#123](https://github.com/cresset-tools/bougie/pull/123))
- *(cli)* bougie composer update --dry-run ([#117](https://github.com/cresset-tools/bougie/pull/117))
- *(fetch)* shared bougie/<v> User-Agent across all outbound HTTP ([#116](https://github.com/cresset-tools/bougie/pull/116))
- *(autoloader)* surface PSR warnings and Composer-style footer ([#113](https://github.com/cresset-tools/bougie/pull/113))
- *(composer-resolver)* pubgrub --lock-verify ([#110](https://github.com/cresset-tools/bougie/pull/110))
- *(cli)* [**breaking**] bougie composer install / fetch rename
- *(cli)* bougie composer dump-autoloader
- *(run)* fall back to default PHP when no project constraint
- *(server)* require --config on all server subcommands; drop XDG default
- *(server)* port bougie-server to Windows via php-cgi.exe

### Fixed

- *(release)* inherit workspace.package.version across all bougie-* crates ([#143](https://github.com/cresset-tools/bougie/pull/143))
- *(tests)* retarget phase9 binary-install tests to `composer fetch`
- *(tests)* pass --config to server list calls in integration tests
- *(sync)* update fragment_name test after mbstring joined baseline

### Other

- extract bougie-babysit into its own crate
