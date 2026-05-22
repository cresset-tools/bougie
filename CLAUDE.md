# bougie

A "uv for PHP": Composer-compatible package manager + PHP toolchain manager
+ dev server, written in Rust. Cross-platform (Linux / macOS / Windows).
The `services` daemon stack (bougied + babysit + sandbox-run) is Unix-only;
`bougie server` runs on Windows via `php-cgi.exe`.

Repo: `cresset-tools/bougie`. Spec sibling: `cresset-tools/php-build-standalone`
(see `SERVER.md`, `DISTRIBUTION.md`, `SERVICES.md` upstream — they are the
contract for index format, server behavior, and service tarballs).

## Repo layout

- `crates/` — 25-member Cargo workspace (see below).
- `flake.nix` — Nix dev env with PHP, curl, bash, coreutils for running
  fixture-generation scripts. Does *not* provide the Rust toolchain (rustup
  handles that via `rust-toolchain.toml`).
- `rust-toolchain.toml` — Pinned Rust **1.95**, `rustfmt` + `clippy`.
- `release-plz.toml` — Git-tag-driven release flow. Not on crates.io; only
  the `bougie` crate gets tagged, leaf crates ride along in GitHub release
  artifacts.
- `.github/workflows/ci.yml` — Linux/macOS/Windows matrix. Windows builds
  only `-p bougie` (sandbox-run is Unix-only and `compile_error!`s elsewhere).
- `.github/workflows/release-plz.yml` — Opens release PRs on `version.rs` /
  `Cargo.lock` changes.
- `.cargo/config.toml` — Defines `cargo t` / `cargo test-all` aliases
  (both run `test --features test-fixtures`).
- `scripts/` — Fixture generators (Composer 2.8.12 phar driven). Run under
  `nix develop`.
- `*_PLAN.md` at repo root — In-flight design docs (see "Plan docs" below).
  Shipped plans get deleted, not archived.
- `target/` — Standard Cargo build dir.

## Workspace crates

**Binary + CLI:**
- `bougie` — Main binary. Dispatch, `shim.rs` (argv[0] routing for
  `bougie-babysit`), `commands/` for subcommand impls.
- `bougie-cli` — clap derive types: `Cli`, `Command`, all subcommand enums.

**Composer ecosystem:**
- `bougie-composer` — `composer.json` / `composer.lock` model + PHP channel
  fetch.
- `bougie-composer-resolver` — Native pubgrub-based Composer dep resolver +
  async metadata fetcher + parallel dist download/extract.
- `bougie-autoloader` — Generates `vendor/composer/autoload_*.php`
  byte-equivalent to Composer 2.8.12. Parallel scan, SIMD classmap.
- `bougie-semver` — Composer-flavored semver (Version, Constraint, Stability)
  with ported composer/semver test cases.
- `bougie-version` — PHP version + Composer-constraint grammar (shared
  between CLI and resolver).
- `bougie-php-json` — Byte-exact PHP `json_encode` for the two flag combos
  Composer relies on (content-hash + `JsonFile::encode`).

**PHP toolchain / package management:**
- `bougie-backend` — Pluggable PHP-distribution backends (bougie index +
  windows.php.net).
- `bougie-index` — Index wire protocol + Sigstore TUF verifier.
- `bougie-fetch` — HTTP downloader with SHA-256 verification + tar.zst/zip
  extraction.
- `bougie-installer` — PHP runtime + extension installer; orchestrates
  download/extract into the install tree.
- `bougie-resolver` — PHP + extension manifest resolver (filters yanked,
  by flavor, picks highest matching).

**Platform / FS / process:**
- `bougie-platform` — Target triple, binfmt, ELF / Mach-O probing.
- `bougie-paths` — XDG / Windows path resolution (`BOUGIE_HOME`,
  `BOUGIE_CACHE`).
- `bougie-fs` — File locks, on-disk state, install-tree layout.
- `bougie-config` — `bougie.toml` / `composer.json` config model + merge.
- `bougie-output` — Text/JSON output channel + tables + progress.
- `bougie-errors` — Domain error taxonomy + exit-code map.

**Server / services:**
- `bougie-daemon` — `bougied` process supervisor: service lifecycle,
  health probes, restarts, provisioning (mariadb, redis, opensearch,
  rabbitmq, mkcert, bougie-server).
- `bougie-babysit` — Per-service babysitter shim spawned by bougied;
  owns the process group, proxies signals, reports exit via socketpair.
- `bougie-server` — Embedded HTTP/FastCGI dev server.
- `bougie-recipe` — Recipe DAG walker for `bougie make` task automation
  (Unix-only, freshness gating).

**Sandboxing (vendored):**
- `sandbox-run` — Cross-platform sandboxing: Linux Landlock (≥5.13)
  via `pre_exec`; macOS SBPL via `pre_exec`. Systemd-style options
  (`protect_system`, `read_only_paths`, `inaccessible_paths`, etc.).
- `macos-sandbox-sys` — FFI bindings for Apple's Sandbox framework
  (`sandbox_init_with_parameters`). Empty stub on non-macOS.

Both vendored (rather than path-dep'd) so the repo is self-contained for
build + audit. Both EUPL-1.2.

## CLI surface

Top-level subcommands (from `bougie-cli`):

- `init [--toml]` — Create a new project.
- `ext {add,remove,list}` — Manage PHP extensions.
- `sync [--offline] [--dry-run]` — Install everything the project requires.
- `up [names...]` / `down [names...] [--purge]` — Start / stop declared
  services.
- `run [--with EXT=VER] [--no-sync] [--xdebug] -- ARGV...` — Run a command
  in the project env; supports ad-hoc extensions and xdebug overlay.
- `php {install,uninstall,list,find,pin,upgrade,dir}` — Manage PHP
  interpreters.
- `composer {install,update,fetch,uninstall,list,find,pin,dir,upgrade,dump-autoloader}`
  — Manage Composer installs.
- `cache {clean,prune,dir,size}` — Cache management.
- `self {update,version}` — Manage the bougie binary.
- `server [--subcommand]` — Dev HTTP/FastCGI server (Run, List, Hosts, Tls).
- `services {add,remove,list,catalog,restart,status,logs,daemon}` — Dev
  services.
- `make [task]` — Recipe DAG walker (`start` alias for `make start`).

Global flags: `--quiet`, `--verbose`, `--format {text,json-v1}`.

## Build / test / lint conventions

- **Tests:** `cargo t` (alias for `cargo test --features test-fixtures`).
  The `test-fixtures` feature gates a `fake-redis` test bin; keeping it
  non-default avoids shipping test infra in installs.
- **Windows tests:** only `-p bougie --lib --test windows_smoke`. Most
  integration tests are Unix-only.
- **Lints:** `unsafe_code = "deny"`, `missing_debug_implementations = "warn"`,
  `clippy::pedantic = "warn"` with opt-outs `module_name_repetitions`,
  `must_use_candidate`, `missing_errors_doc`.
- **Unsafe policy:** one allowlisted `#[allow(unsafe_code)]` site in
  `bougie-daemon/src/daemon/supervisor.rs` (pre_exec setup: `libc_dup2`
  for socket fd 3 + `sandbox_run::apply_sandbox`). Adding any new
  allowlist requires explicit review.
- **Release profile:** `codegen-units = 1`, `lto = "fat"`,
  `panic = "abort"`, `strip = "symbols"`.
- **`profiling` profile:** inherits `release` with line tables, no strip.
- **No Justfile / Makefile.** Plain Cargo + shell scripts.
- **Edition 2024**, MSRV 1.95.

## Release flow

Conventional commits (`feat:`, `fix:`, `chore:`, etc.) drive release-plz.
On `version.rs` / `Cargo.lock` changes, `release-plz.yml` opens a release
PR that bumps the version and updates `CHANGELOG.md`. Merging the PR tags
`v<version>` on the `bougie` crate and creates a GitHub release.

- `publish = false` everywhere — bougie is not on crates.io.
- `release = false` on all leaf + vendored crates; only `bougie` gets
  tagged. Leaf changes only flow to releases via that tag.
- `git_only = true` for `bougie` — no crates.io comparison.

**Use conventional commit prefixes** or release-plz won't pick the
change up. Examples in `git log`: `feat(composer-resolver): ...`,
`fix(release): ...`, `perf(...)`, `refactor(...)`, `test(...)`,
`docs(...)`, `chore: ...`.

## Plan docs (in-flight)

- `AUTOLOADER_PLAN.md` — `bougie composer dump-autoloader` native port.
  Mostly shipped; remaining: platform-check emit + wire-up (the last
  Composer-parity gap).
- `RESOLVER_PLAN.md` — Native pubgrub-based Composer resolver. Largely
  shipped; ongoing work in `bougie-composer-resolver`.
- `RESOLVER_TEST_PLAN.md` — Resolver test architecture (composer/semver
  conformance, fixtures, cross-check, derivation snapshots).
- `SERVER_PLAN.md` — `bougie server` per upstream `SERVER.md`. 8-phase
  bottom-up build.
- `TOOL_PLAN.md` — `bougie tool` (uv-tool-style globally-installed
  isolated PHP CLI tools). Design only; no implementation yet.

When a plan ships, **delete** the file rather than archiving it. The repo
root is for current work; shipped plans live in git history.

## External specs

`cresset-tools/php-build-standalone` is the spec source for:
- **`SERVER.md`** — `bougie server` HTTP/FastCGI behavior.
- **`DISTRIBUTION.md`** — Index manifest shape, blob kinds, closure
  semantics.
- **`SERVICES.md`** — Service catalog model.

Bougie consumes those specs; changes that affect wire format need
coordinated PRs.

## Invariants and conventions

- **No Composer plugins, no `pre-/post-*` scripts.** Bougie never runs
  them. When users need a plugin's behavior (composer/installers paths,
  Symfony Flex recipes, Laravel package discovery), reimplement it
  natively. See `bougie-recipe` for the Flex-recipe lane.
- **Byte-equivalent Composer output where applicable.**
  `bougie-autoloader` matches Composer 2.8.12's `autoload_*.php`
  byte-for-byte across the fixture suite. `bougie-php-json` exists
  solely to match PHP's `json_encode` byte output for content hashing.
- **Conventional commits required** (see Release flow).
- **Sandbox-by-default for spawned services.** Anything bougied
  spawns goes through `bougie-babysit` + `sandbox-run`.
- **Vendored crates stay self-contained.** Don't replace `sandbox-run`
  / `macos-sandbox-sys` with path-deps to sibling repos.

## Where to look first

- New CLI flag: `crates/bougie-cli/src/lib.rs`.
- New subcommand impl: `crates/bougie/src/commands/`.
- Composer-side change: `crates/bougie-composer{,-resolver}/`.
- Autoloader change: `crates/bougie-autoloader/` + run
  `scripts/generate-autoload-fixtures.sh` if expected output shifts.
- Server / FastCGI: `crates/bougie-server/`.
- Service supervision / sandboxing: `crates/bougie-daemon/`,
  `crates/bougie-babysit/`, `crates/sandbox-run/`.
- Index / TUF: `crates/bougie-index/`, `crates/bougie-fetch/`.
