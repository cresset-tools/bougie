# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/cresset-tools/bougie/releases/tag/bougie-autoloader-v0.1.0) - 2026-05-22

### Added

- *(autoloader)* surface PSR warnings and Composer-style footer ([#113](https://github.com/cresset-tools/bougie/pull/113))
- *(autoloader)* --apcu-autoloader + config.autoloader-suffix
- *(autoloader)* emit installed.json + installed.php
- *(autoloader)* autoload_static.php emit (Phase 3 part 3)
- *(autoloader)* autoload_real.php emit (Phase 3 part 2)
- *(autoloader)* vendored runtime files (Phase 3 part 1)
- *(autoloader)* exclude-from-classmap + classmap-exclude/mixed fixtures
- *(autoloader)* --optimize + --classmap-authoritative flags
- *(autoloader)* classmap scanner + emitter (Phase 2 part 1)
- *(autoloader)* Phase 1 — PSR-4 / PSR-0 / files emitters
- *(autoloader)* bougie-autoloader skeleton + byte-equivalence fixtures

### Fixed

- *(release)* inherit workspace.package.version across all bougie-* crates ([#143](https://github.com/cresset-tools/bougie/pull/143))
- *(autoloader)* emit files autoload in topological order
- *(autoloader)* vendor-dir auto-exclude on PSR-* scans that span vendor
- *(autoloader)* port reverse-sortPackageMap order for PSR-* + classmap
- *(autoloader)* apply krsort to PSR-* emit + classmap scan
- *(autoloader)* canonicalize install paths so macOS /var/folders works
- *(autoloader)* dump_bench copy tolerates dangling symlinks
- *(autoloader)* dump_bench example must not mutate the target tree

### Other

- Merge pull request #101 from cresset-tools/chore/dump-bench-optimize-flag
- *(bench)* dump_bench takes -o / --optimize
- Merge pull request #97 from cresset-tools/feat/autoloader-version-normalize
- extract bougie-php-json from bougie-composer
- *(autoloader)* failing fixture for files-autoload topological order
- *(autoloader)* dump_bench errors always carry path + operation
- *(autoloader)* dump_bench can compare against composer dump-autoload
- *(autoloader)* example binary to time dump_autoload on real projects
- *(autoloader)* parallelize classmap scan + bench harness
- *(autoloader)* extract collect module + use md-5 crate
