# bougie

A "uv for PHP": Composer-compatible package manager + PHP toolchain manager
+ dev server, written in Rust. Cross-platform (Linux / macOS / Windows).
The `services` daemon stack (bougied + babysit + sandbox-run) is Unix-only;
`bougie server` runs on Windows via `php-cgi.exe`.

Repo: `cresset-tools/bougie`. Spec sibling: `cresset-tools/php-build-standalone`
(see `SERVER.md`, `DISTRIBUTION.md`, `SERVICES.md` upstream — they are the
contract for index format, server behavior, and service tarballs).

## Repo layout

- `crates/` — 28-member Cargo workspace (see below).
- `flake.nix` — Nix dev env with PHP, curl, bash, coreutils for running
  fixture-generation scripts. Does *not* provide the Rust toolchain (rustup
  handles that via `rust-toolchain.toml`).
- `rust-toolchain.toml` — Pinned Rust **1.95**, `rustfmt` + `clippy`.
- `release-please-config.json` / `.release-please-manifest.json` —
  release-please config. Single-component "bougie" package. Each
  `[package]` block inherits `[workspace.package].version` via
  `version.workspace = true`; the version literals release-please
  rewrites are all annotated with `# x-release-please-version` (the
  workspace version plus one per `[workspace.dependencies]` entry — see
  Release flow). All crates publish to crates.io on the `bougie-v<version>`
  tag.
- `.github/workflows/ci.yml` — Linux/macOS/Windows matrix. Windows builds
  only `-p bougie` (sandbox-run is Unix-only and `compile_error!`s elsewhere).
- `.github/workflows/release-please.yml` — Opens release PRs from
  conventional commits, refreshes `Cargo.lock` on the PR branch, and
  on PR merge pushes the `bougie-v<version>` tag. release-please
  itself runs with `skip-github-release: true` so dist
  (`bougie-release.yml`) owns the GitHub Release.
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

- `init [--toml] [--name VENDOR/PACKAGE]` — Create a new project in the
  current directory.
- `new DIRECTORY [--toml] [--name VENDOR/PACKAGE]` — Create DIRECTORY under
  the cwd and scaffold a new project inside it.
- `ext {add,remove,list}` — Manage PHP extensions.
- `add <pkgs> [--dev] [--no-sync] [--frozen]` / `remove <pkgs>` — uv-style
  top-level twins of `composer require`/`remove`. `add` uses the `@`
  supply syntax (`vendor/pkg@^1.0`) and a `>=X.Y` lower-bound default
  (vs `composer require`'s caret); shared engine, [`DefaultConstraint`]
  policy selects the default. `--frozen` = edit composer.json only;
  `--no-sync` = re-lock but don't install.
- `tree [PACKAGE] [--no-dev]` — native dependency tree (uv's `uv tree`);
  delegates to the `composer show --tree` renderer.
- `outdated [pkgs] [--direct] [--major/minor/patch-only] [--strict]` —
  native outdated check; same engine as `composer outdated`.
- `sync [--offline] [--dry-run]` — Install everything the project requires.
- `up [names...]` / `down [names...] [--purge]` — Start / stop declared
  services.
- `run [--with EXT=VER] [--no-sync] [--xdebug] -- ARGV...` — Run a command
  in the project env; supports ad-hoc extensions and xdebug overlay.
- `php {install,uninstall,list,find,pin,upgrade,dir}` — Manage PHP
  interpreters.
- `composer {install,update,require,remove,show,why,why-not,outdated,audit,licenses,fund,status,validate,dump-autoload}`
  — Native, Composer-compatible command surface (the uv-pip model). bougie
  does **not** bundle or execute the Composer phar; every listed verb is a
  native reimplementation. An unrecognized subcommand
  (`create-project`, `archive`, `bump`, …) errors with a pointer to
  `bougie tool install composer/composer`. The `composer` shim symlink in
  `.bougie/bin/` (and the global one in the tool bin dir) routes to these
  native subcommands, so `composer install` from a recipe or any shell runs
  bougie's native installer.
- `cache {clean,prune,dir,size}` — Cache management.
- `self {update,version}` — Manage the bougie binary.
- `server [NAME]` — Dev HTTP/FastCGI server. With no subcommand it's
  the project verb: register the current project with the shared
  bougied-managed server, print its `<name>.bougie.run` URL, and attach
  to its (host-scoped) log (Ctrl-C detaches). Subcommands: `run`
  (low-level primitive against an explicit `--config server.toml`, what
  bougied spawns), `status` (live host/pool table via control socket,
  `list` alias), `open`, `stop`, `logs`, `tls`, `hosts`. See
  `SERVER_CLI_PLAN.md`.
- `services {add,remove,list,projects,catalog,restart,status,logs,daemon}` —
  Dev services. `projects` lists every provisioned tenant across the shared
  services and the owning project (reads the on-disk tenant ledgers; no daemon);
  `projects purge` deprovisions tenants (orphaned-by-default, or `--project`/
  `--all`) — destructive, so it confirms unless `--yes`/`--dry-run`.
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

Conventional commits (`feat:`, `fix:`, `chore:`, etc.) drive
release-please. On every push to `main`, release-please scans commits
since the last `bougie-v*` tag and opens/updates a release PR that
bumps every annotated version pin and prepends to `CHANGELOG.md`.
Merging the PR tags `bougie-v<version>`, which triggers
`bougie-release.yml` (cargo-dist) to build the binary matrix.

- **Single component, lockstep versioning.** `release-please-config.json`
  declares one package (`"."`, component `bougie`). Every workspace member
  inherits `workspace.package.version` (or is bumped via an explicit pin)
  on the same release cadence — the unified version isn't semantically
  meaningful, it just means a leaf-only `fix:` commit still drives a
  top-level release. This is the uv approach (their internal crates sit
  at `0.0.X` while `uv` is at `0.11.X`); bougie keeps everything at the
  bougie version.
- **Version literals are annotated, not avoided.** `[package]` blocks
  inherit `[workspace.package].version` via `version.workspace = true`,
  so they hold no literal. But every `[workspace.dependencies]` entry
  carries both `path` *and* a `version = "..."` requirement — cargo
  rejects a published crate whose normal deps lack a version, and there
  is no `version.workspace = true` for a dependency's version field. So
  each is a literal, tagged with `# x-release-please-version` so
  release-please bumps it in lockstep with the workspace version.
  `crates/bougie/Cargo.toml` is a release-please `extra-file` (alongside
  root `Cargo.toml`) so its inline literals get bumped too. If you add a
  new internal dep, annotate its `version`.
- **One `default-features = false` exception** in
  `crates/bougie/Cargo.toml` keeps an inline `bougie-index = { path =
  "...", version = "...", default-features = false }` block: cargo
  rejects member-level `default-features = false` overrides when the
  workspace entry has defaults on, so the bin bypasses workspace
  inheritance for that one dep. It carries its own annotated version
  literal.
- **Cargo.lock is refreshed in-workflow.** release-please doesn't
  understand `Cargo.lock`; the workflow runs `cargo update --workspace
  --offline` on the PR branch and pushes the result so CI builds aren't
  broken by a stale lockfile.
- **Published to crates.io.** All 28 crates publish on the
  `bougie-v<version>` tag via `.github/workflows/crates-publish.yml`
  (`cargo ws publish --publish-as-is`, topo-ordered, idempotent re-run).
  Needs a `CARGO_REGISTRY_TOKEN` repo secret. cargo-dist
  (`bougie-release.yml`) independently owns the GitHub Release binaries
  on the same tag. Test fixtures are `exclude`d from the heavier crates
  (bougie-autoloader, bougie-composer-resolver, the `bougie` bin) to
  keep packed crates lean.

**Use conventional commit prefixes** or release-please won't pick the
change up. Examples in `git log`: `feat(composer-resolver): ...`,
`fix(release): ...`, `perf(...)`, `refactor(...)`, `test(...)`,
`docs(...)`, `chore: ...`.

## Plan docs (in-flight)

- `RESOLVER_PLAN.md` — Native pubgrub-based Composer resolver. Largely
  shipped; ongoing work in `bougie-composer-resolver`.
- `RESOLVER_TEST_PLAN.md` — Resolver test architecture (composer/semver
  conformance, fixtures, cross-check, derivation snapshots).
- `SERVER_PLAN.md` — `bougie server` engine per upstream `SERVER.md`.
  8-phase bottom-up build (shipped).
- `SERVER_CLI_PLAN.md` — `bougie server` CLI surface redesign: the
  project verb over the shared daemon + `status`/`open`/`stop`/`logs`,
  plus a standalone foreground fallback for Windows. All phases shipped
  (Windows path compile-verified, runtime pending CI); delete once CI
  confirms the Windows build.
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

- **No Composer plugins.** Bougie never runs plugin install-time hooks.
  When users need a plugin's behavior (composer/installers paths, Symfony
  Flex recipes, Laravel package discovery), reimplement it natively. See
  `bougie-recipe` for the Flex-recipe lane and `bougie-installers` for the
  declarative-plugin ports.
- **Root `composer.json` scripts: opt-in, off by default.** Composer only
  ever runs scripts from the *root* package (never dependencies), so they
  are the project author's own commands. `bougie-scripts` runs them when
  enabled via `[scripts] run = true` / `--scripts` (parse → classify →
  dispatch). The default stays deterministic-native: scripts don't run,
  and bougie reproduces the effect of the ones it can (Laravel discovery
  via `bougie-installers`). PHP-callback entries (`Class::method`) are
  warn-skipped except a small native allowlist. The resolver fires
  lifecycle hooks (`ScriptHooks`) during `install_from_lock`; the CLI
  (`commands/scripts.rs`) builds the `ScriptContext`.
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
