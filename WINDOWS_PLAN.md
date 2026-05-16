# Windows support via windows.php.net

## Goal

Make `bougie php install`, `bougie ext add`, `bougie sync`, and
`bougie run` work on native Windows by consuming PHP's official
Windows distribution at <https://windows.php.net>, instead of waiting
on a Windows port of `php-build-standalone`.

The user-facing contract stays the same:

```
bougie php install 8.4
bougie ext add xdebug redis
bougie run -- php -v
```

The mechanism beneath it changes: on Windows, bougie talks to
windows.php.net (interpreter ZIPs + PECL DLL ZIPs) instead of to a
Bougie-format index served from `index.bougie.tools`.

## Non-goals

- **Service supervisor.** `bougied`, `bougie services *`, MariaDB /
  Redis / OpenSearch / RabbitMQ orchestration — all Unix-only,
  Landlock/SBPL-gated. Stay disabled on Windows.
- **Content-addressed deduplication.** windows.php.net DLLs are not
  content-hashed; two interpreter installs each carry their own copy
  of `libssl-3-x64.dll` next to `php.exe`. The store/ directory still
  exists for extension DLLs, but DLL-level dedup against the
  interpreter is best-effort. This is the price of using an upstream
  we don't control; revisit if we ever publish our own Windows index.
- **Sigstore verification.** windows.php.net publishes SHA-256 in
  `releases.json` and that's it. Verification on Windows is
  `TLS + sha256-from-releases.json`. Document the trade.
- **Cross-VC ABI matrixing.** A given PHP minor pins to a single VC
  runtime (8.4 → vs17, 8.0–8.3 → vs16, …). bougie derives VC from
  PHP minor; the user never picks it.

## Approach

Introduce a `Backend` trait so the install / sync code paths stay
OS-agnostic:

```rust
trait Backend {
    fn resolve_php(&self, spec: &VersionLike, flavor: Flavor)
        -> Result<PhpRecipe>;
    fn resolve_extension(&self, name: &str, php_minor: PartialVersion,
                         flavor: Flavor, version_pin: Option<&str>)
        -> Result<ExtRecipe>;
    fn list_available_extensions(&self, php_minor: PartialVersion,
                                 flavor: Flavor)
        -> Result<Vec<AvailableExt>>;
}

struct PhpRecipe {
    version: Version,
    blob_url: String,
    blob_sha256: String,
    blob_size: u64,
    archive: ArchiveKind,        // TarZst | Zip
    layout: LayoutKind,          // BougieTree | WindowsFlat
}
```

Two implementations:

- `BougieIndexBackend` — today's behaviour, exact rewrap of the code
  currently in `src/install.rs` + `src/index/`.
- `WindowsPhpNetBackend` — new.

Backend selection happens once, at `install_php` / `install_extension`
entry, keyed on `Triple::detect().os`. Everything downstream
(`Paths`, `store::install_dir`, `conf_d::write_ext_fragment`,
`commands::run`) operates on `Recipe` outputs and stays unchanged.
The integration-test harness gets a `MockBackend` for free.

## The windows.php.net surface

### Interpreter

```
https://windows.php.net/downloads/releases/releases.json
```

Schema (per version → flavor → ZIP):

```json
{
  "8.4": {
    "version": "8.4.3",
    "date": "2025-01-30",
    "ts-vs17-x64":     {"path": "/downloads/releases/php-8.4.3-Win32-vs17-x64.zip",
                        "sha256": "..."},
    "nts-vs17-x64":    {"path": "/downloads/releases/php-8.4.3-nts-Win32-vs17-x64.zip",
                        "sha256": "..."},
    "ts-vs17-arm64":   {...},
    "nts-vs17-arm64":  {...}
  },
  "8.3": {...},
  ...
}
```

(Exact key spelling needs verification at implementation time —
treat the schema above as illustrative, not authoritative. The plan
hinges on "there is some structured JSON that lists versions + SHA-256",
which there is.)

ZIP layout (top-level dir):

```
php-8.4.3/
  php.exe
  php-cgi.exe
  php8apache2_4.dll
  php8.dll                     # engine
  libssl-3-x64.dll
  libcrypto-3-x64.dll
  icudt74.dll
  libcurl.dll
  libsasl.dll
  ...
  ext/
    php_curl.dll
    php_openssl.dll
    php_mbstring.dll
    php_intl.dll
    ...
  php.ini-development
  php.ini-production
  pear/
```

### PECL

```
https://windows.php.net/downloads/pecl/releases/<extname>/<version>/
```

contains:

```
php_<extname>-<extver>-<phpminor>-<ts|nts>-<vc>-<arch>.zip
php_<extname>-<extver>-<phpminor>-<ts|nts>-<vc>-<arch>.zip.sha256
```

ZIP layout (flat):

```
php_xdebug.dll
xdebug.ini   (sample, often)
LICENSE
```

Some extensions ship extra DLLs (imagick → CORE_RL_*.dll). Those
must end up on PHP's DLL search path.

## Phase breakdown

### Phase 1 — target + archive plumbing (1 PR)

Touches: `src/target.rs`, `src/fetch.rs`, `Cargo.toml`.

- `Os::Windows`, `Vendor::Pc`, `Env::Msvc` added. `Triple::detect()`
  on `cfg(windows)` returns `x86_64-pc-windows-msvc` (or `aarch64-`).
- `ArchiveKind { TarZst, Zip }`. Extract path in `fetch.rs` switches
  on it. The `zip` crate is already a dep (unzip-shim).
- `Paths::from_env()` picks the native `etcetera` strategy per
  target (`Xdg` on Unix, `Windows` on Windows). On Windows both the
  data and cache slots anchor under `%LOCALAPPDATA%` (via
  `Windows::cache_dir`); `%APPDATA%/Roaming` is deliberately avoided
  so bougie's multi-GB `installs/` tree doesn't get dragged into
  domain roaming profiles.
- `cfg(unix)` gate the `target.rs` `read_pt_interp` path; Windows
  doesn't need libc detection.

Verifiable independently: `Triple::detect()` returns the right
string on a Windows host; a roundtrip ZIP extract test passes.

### Phase 2 — Backend trait + bougie-index extraction (1 PR)

Touches: `src/install.rs`, new `src/backend/` module tree.

- Move the existing index walk out of `install_php` into
  `backend::bougie::BougieIndexBackend::resolve_php`.
- `install_php` becomes ~20 lines: detect target → pick backend →
  call `backend.resolve_php(...)` → call `fetch::extract_recipe(...)`.
- All wiremock-based tests stay green by injecting a
  `BougieIndexBackend` pointing at the wiremock URL.

Refactor only. No behaviour change. Lands before any Windows code so
the diff stays small.

### Phase 3 — `WindowsPhpNetBackend` interpreter path (1 PR)

Touches: `src/backend/windows_php_net.rs` (new).

- Fetch `releases.json`, parse, filter by flavor (`ts`/`nts`,
  `+debug`), arch (host arch), VC (derived from PHP minor:
  `vs17` for 8.4+, `vs16` for 8.0–8.3).
- `resolve_php` returns a `PhpRecipe` with `archive: Zip,
  layout: WindowsFlat`.
- `fetch::extract_recipe` with `layout: WindowsFlat` does a flat
  extract + relayout:
  - Top-level `*.exe`, `*.dll`, `pear/` → `<install>/bin/`
  - `ext/php_*.dll` → `<install>/lib/extensions/` (drop the `php_`
    prefix so the conf.d emitter can keep generating
    `extension=curl` rather than `extension=php_curl`)
  - `php.ini-development` → `<install>/etc/php/php.ini`
- ETag caching mirrors `index::fetch::fetch_root` — cache
  `releases.json` under `cache/index/windows.php.net/`, revalidate
  via `If-None-Match`.

Acceptance: `bougie php install 8.4` produces a working tree;
`bougie run -- php -v` prints the version.

### Phase 4 — PECL extensions (1 PR)

Touches: `src/backend/windows_php_net.rs`, `src/baseline.rs`.

- `resolve_extension` builds the URL deterministically from
  `(name, php_minor, flavor, vc, arch)`. The version is the
  awkward part — there's no canonical JSON index per extension.
  Options, in increasing order of work:
  1. **Hardcode known-good versions per extension per bougie release.**
     Ship a `BUNDLED_PECL_VERSIONS: &[(name, php_minor, version)]`
     table; refresh on each bougie release. Mirror what
     `baseline.rs` already does for Linux/Darwin baselines.
  2. **Parse the HTML directory listing** at
     `pecl/releases/<name>/` to extract the latest version. Brittle
     (HTML can change) but version-agnostic.
  3. **Use PECL.php.net's REST API** (`/rest/r/<name>/allreleases.xml`)
     to discover versions, then check availability on
     windows.php.net by HEAD. Most correct, most code.

  Recommend (1) for the first ship — adequate for the baseline
  extension set (xdebug, redis, imagick, igbinary, msgpack, apcu,
  pcov, mongodb) and lets us ship before solving discovery.
- Fetch `<zip>.sha256` sidecar, verify against the downloaded ZIP.
- Place the extension as
  `<store>/ext-<name>-<ver>+php<minor>-<flavor>-<sha8>/<name>.dll`.
- Write conf.d fragment with absolute path:
  `extension=C:\Users\...\store\ext-xdebug-...\xdebug.dll`.
  PHP on Windows accepts absolute paths for `extension=` (and
  `zend_extension=`).

Acceptance: `bougie ext add xdebug` on a Windows host installs and
loads xdebug under `php -m`.

### Phase 5 — dependent-DLL handling (1 PR)

Touches: `src/backend/windows_php_net.rs`, `src/install.rs`.

Some extensions ship extra DLLs in their ZIP. Strategy:

- Move the extension's main DLL into the store path as in Phase 4.
- Copy every other `.dll` in the ZIP into the
  *interpreter install's* `bin/` directory (next to `php.exe`),
  with a sidecar manifest recording which DLLs came from which
  extension so `bougie ext remove` can clean up.
- Conflicts (two extensions shipping `magickwand.dll` at different
  versions) are detected at install time; the second install fails
  with a structured error pointing at the conflict.

This is the only place where the Windows DLL search model bleeds
into bougie state. It's contained.

### Phase 6 — `PHP_INI_SCAN_DIR` separator + smoke (1 PR)

Touches: `src/conf_d.rs`, `tests/`.

- `conf_d::php_ini_scan_dir` currently hardcodes `:`. Switch to
  `cfg(windows) ⇒ ';' ⇒ ':'`.
- Add `tests/windows_smoke.rs` (gated `#[cfg(windows)]`) covering
  install → ext add → run → `php -m` shows the extension.
- CI: GitHub Actions `windows-latest` runner, runs the new smoke
  test plus the existing unit tests with the network paths
  disabled.

## Open questions

- **Exact `releases.json` schema.** Phase 3 implementation should
  start by curling it and printing it; the structure above is the
  best recollection but unverified. If keys differ, adjust the
  parser; nothing else moves.
- **PECL `.sha256` sidecar coverage.** Are sidecars present for
  *every* extension or just popular ones? Falling back to "no
  sidecar → fail-closed" forces users to pin extensions known to
  ship sidecars; falling back to "no sidecar → TLS-only" weakens
  the trust story. Resolve before merging Phase 4.
- **Debug builds.** windows.php.net publishes `-debug-` variants;
  bougie's `nts-debug` / `zts-debug` flavors map directly. The
  URL convention is `php-<ver>-<nts->Win32-<vc>-<arch>.zip`
  vs `php-<ver>-<nts->Win32-<vc>-<arch>-debug-pack.zip` — the
  debug-pack is *separate* from the runtime ZIP and contains PDBs,
  not a different php.exe. For bougie's flavor semantics, "debug"
  means "PHP built `--enable-debug=yes`" which on Windows isn't
  what windows.php.net ships in the debug-pack. We may need to
  drop `*-debug` flavors on Windows (windows.php.net doesn't
  publish a `--enable-debug` build of mainline PHP) and document
  that.
- **ARM64 readiness.** windows.php.net ships ARM64 for 8.4+;
  8.0–8.3 are x64-only. Resolution path needs to fall back to a
  "no match" error with a clear message when the user is on
  ARM64 and asks for 8.3.
- **`bougied` strategy on Windows.** Long-term, services need a
  Windows answer (named pipes? Job objects? plain service-less
  spawn?). Out of scope here. The `services` subcommand should
  exit with a `not-supported-on-windows` error rather than
  silently hanging.

## What does NOT move

- `Paths` / `store::install_dir` / `conf_d` core logic.
- `request.rs` request grammar.
- `composer/` subdirectory (composer is platform-agnostic; the
  composer phar runs on any PHP).
- `state.rs`, `lock.rs` (file locking uses `std::fs::File::try_lock`,
  which is cross-platform — BSD `flock(2)` on Unix, `LockFileEx` on
  Windows — so neither file needs a Windows port). The
  `rustix::fs::flock` import in `src/daemon/mod.rs` is `bougied`-only
  and stays `cfg(unix)`.
- The CLI surface in `cli.rs`.

The Backend trait is the only structural change; everything else
is implementation behind it. That's the test of whether this plan
is right — if Phase 2 (the trait extraction) starts requiring
changes to `Paths` or `conf_d` or `state`, the abstraction is
wrong and we should rethink before pushing Phase 3.
