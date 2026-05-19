# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.4.0...bougie-v0.5.0) - 2026-05-19

### Added

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

- *(tests)* retarget phase9 binary-install tests to `composer fetch`
- *(tests)* pass --config to server list calls in integration tests
- *(sync)* update fragment_name test after mbstring joined baseline

### Other

- extract bougie-babysit into its own crate
