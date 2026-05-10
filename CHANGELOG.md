# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/cresset-tools/bougie/releases/tag/v0.1.0) - 2026-05-10

### Other

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
