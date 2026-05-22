# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/cresset-tools/bougie/releases/tag/bougie-composer-resolver-v0.1.0) - 2026-05-22

### Added

- *(composer-resolver)* solve-phase progress spinner + tracing logs ([#137](https://github.com/cresset-tools/bougie/pull/137))
- *(composer-resolver)* parallel metadata pre-fetch closure ([#136](https://github.com/cresset-tools/bougie/pull/136))
- *(composer-resolver)* send per-host auth on dist downloads ([#134](https://github.com/cresset-tools/bougie/pull/134))
- *(composer-resolver)* support Composer v1 repositories ([#133](https://github.com/cresset-tools/bougie/pull/133))
- *(composer-resolver)* http-basic + bearer auth for composer-type repos ([#131](https://github.com/cresset-tools/bougie/pull/131))
- *(composer-resolver)* support composer.json `repositories` field (composer-type) ([#130](https://github.com/cresset-tools/bougie/pull/130))
- *(semver)* Constraint::parse handles dev-<name> branch references ([#129](https://github.com/cresset-tools/bougie/pull/129))
- *(composer-resolver)* wildcard replace/provide via on-demand synthesis ([#127](https://github.com/cresset-tools/bougie/pull/127))
- *(composer-resolver)* prefer-stable candidate ordering ([#126](https://github.com/cresset-tools/bougie/pull/126))
- *(semver)* Constraint::parse handles Nx-dev + commit-ref + @stability suffixes ([#125](https://github.com/cresset-tools/bougie/pull/125))
- *(composer-resolver)* virtual packages via provide/replace pre-fetch ([#124](https://github.com/cresset-tools/bougie/pull/124))
- *(composer-resolver)* write composer.lock from bougie composer update ([#123](https://github.com/cresset-tools/bougie/pull/123))
- *(composer-resolver)* consult /p2/<name>~dev.json when dev versions allowed ([#121](https://github.com/cresset-tools/bougie/pull/121))
- *(composer-resolver)* minimum-stability + per-package @stability flags ([#120](https://github.com/cresset-tools/bougie/pull/120))
- *(composer-resolver)* encode replace/provide as additional requires ([#119](https://github.com/cresset-tools/bougie/pull/119))
- *(cli)* bougie composer update --dry-run ([#117](https://github.com/cresset-tools/bougie/pull/117))
- *(composer-resolver)* pubgrub DependencyProvider over Packagist ([#115](https://github.com/cresset-tools/bougie/pull/115))
- *(fetch)* shared bougie/<v> User-Agent across all outbound HTTP ([#116](https://github.com/cresset-tools/bougie/pull/116))
- *(composer-resolver)* Packagist v2 metadata fetcher ([#114](https://github.com/cresset-tools/bougie/pull/114))
- *(composer-resolver)* pubgrub --lock-verify ([#110](https://github.com/cresset-tools/bougie/pull/110))
- *(composer-resolver)* install_from_lock orchestrator
- *(fetch)* detect_zip_top_level + DistRequest auto-detect
- *(composer-resolver)* add parallel dist downloader

### Fixed

- *(release)* inherit workspace.package.version across all bougie-* crates ([#143](https://github.com/cresset-tools/bougie/pull/143))
- *(composer-resolver)* union repo candidates and multi-provider virtuals ([#135](https://github.com/cresset-tools/bougie/pull/135))

### Other

- *(composer-resolver)* make prefetch fan-out async-native ([#144](https://github.com/cresset-tools/bougie/pull/144))
- *(composer-resolver)* mem::forget provider + hoist virtual computation into workers ([#142](https://github.com/cresset-tools/bougie/pull/142))
- *(composer-resolver)* hand pubgrub a Ref instead of cloning versions_for ([#140](https://github.com/cresset-tools/bougie/pull/140))
- *(composer-resolver)* regression for many provider versions replacing one virtual ([#139](https://github.com/cresset-tools/bougie/pull/139))
- *(composer-resolver)* cache parsed Version with LockPackage ([#122](https://github.com/cresset-tools/bougie/pull/122))
