# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/cresset-tools/bougie/releases/tag/bougie-semver-v0.1.0) - 2026-05-22

### Added

- *(semver)* Constraint::parse handles dev-<name> branch references ([#129](https://github.com/cresset-tools/bougie/pull/129))
- *(semver)* Constraint::parse handles Nx-dev + commit-ref + @stability suffixes ([#125](https://github.com/cresset-tools/bougie/pull/125))
- *(composer-resolver)* pubgrub DependencyProvider over Packagist ([#115](https://github.com/cresset-tools/bougie/pull/115))
- *(composer-resolver)* pubgrub --lock-verify ([#110](https://github.com/cresset-tools/bougie/pull/110))
- *(semver)* Composer-conformant Version + Constraint impl ([#104](https://github.com/cresset-tools/bougie/pull/104))
- *(semver)* bougie-semver crate skeleton + Layer 1 conformance fixture
