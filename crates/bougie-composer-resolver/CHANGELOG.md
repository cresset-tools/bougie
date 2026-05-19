# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/cresset-tools/bougie/releases/tag/bougie-composer-resolver-v0.1.0) - 2026-05-19

### Added

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

### Other

- *(composer-resolver)* cache parsed Version with LockPackage ([#122](https://github.com/cresset-tools/bougie/pull/122))
