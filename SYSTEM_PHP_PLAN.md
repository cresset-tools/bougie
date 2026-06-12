# System PHP support — plan

Let bougie discover and use a PHP already installed on the machine
instead of always downloading a bougie-managed build — uv's system-Python
model (`--managed-python` / `--no-managed-python` /
`--no-python-downloads`), adapted to PHP.

## The honest constraint (read this first)

bougie's core value is managing **PHP + extensions** together: it ships
prebuilt `.so` extensions compiled against *its own* PHP builds, matched
by `(php_minor, flavor, zend_module_api)`. A **system PHP is a foreign
build** (distro-patched, unknown/!= ABI), so bougie generally **cannot
install its prebuilt extensions onto it** safely.

So system PHP is scoped as **"bring your own, fully formed"**: bougie
*discovers, validates, and uses* it, but does **not** manage extensions
on it. A project's required `ext-*` must already be loaded in the system
PHP (verified by probing); if one is missing, bougie errors with guidance
rather than installing an ABI-mismatched `.so`. Opportunistic extension
install onto an ABI-matching system PHP is a deferred follow-up (below).

This mirrors the managed/system split honestly: managed PHP = full
toolchain (PHP + extensions, reproducible); system PHP = use what's there.

## Preference model (uv's exact model)

uv's boolean flags — no `--php-preference` enum:

- `--managed-php` — require a bougie-managed PHP (never use system).
- `--no-managed-php` — never use a managed PHP (require system).
- `--no-php-downloads` — don't download a managed PHP (use an installed
  managed one, or system).

| flags | order tried |
|--|--|
| *(none — **default**)* | managed-**installed** → system (if it qualifies) → managed-**download**. |
| `--managed-php` | managed-installed → managed-download. Never system. |
| `--no-managed-php` | system only; error if it doesn't qualify. |

This is uv's default `Managed` semantics verbatim (verified against
`uv-python/src/discovery.rs`, `enum PythonPreference`, `#[default] Managed`):
*"Prefer managed Python installations over system Python installations.
System Python installations are still preferred over downloading managed
Python versions."* So: an already-installed managed PHP of the right
version wins; failing that, an adequate **already-present** system PHP is
used **before** paying for a download; only if neither is present do we
download a managed build.

A **system candidate qualifies only if** it satisfies the version
constraint, the required flavor, **and already loads every required
`ext-*`** (see Selection). The instant the system PHP is missing a
required extension it's disqualified, and selection continues to
managed-download — which *can* install the extension. This ext-gate is
what makes using a system PHP safe: you never get stuck on one that can't
satisfy the project.

`--no-php-downloads` is combinable: it drops the "managed-download" step
(offline / air-gapped / CI with a preinstalled PHP). E.g. default +
`--no-php-downloads` = managed-installed → system → error.

**Behavioral note:** today bougie always downloads/uses managed. Under
this default a fresh machine with no managed install but a qualifying
system PHP will use the system PHP instead of downloading — matching uv.
Reproducibility-sensitive users pin to managed with `--managed-php`
(or `[php] managed = true`).

Internally a small enum mirroring uv (`OnlyManaged | Managed (default) |
OnlySystem`), derived from the flags + config. Config keys (merged like
`version`/`flavor`): `[php] managed` (`Option<bool>` — `true` ⇒
`OnlyManaged`, `false` ⇒ `OnlySystem`, unset ⇒ `Managed` default) and
`[php] downloads` (`Option<bool>` — `false` ⇒ no downloads).

## Discovery + probe

**Discovery** — gather candidate php binaries:
- `PATH` entries named `php`, plus version-suffixed Debian/Homebrew forms
  (`php8.3`, `php83`).
- Known locations: `/usr/bin`, `/usr/local/bin`, Homebrew
  (`/opt/homebrew/opt/php@*/bin`, `/usr/local/opt/php@*/bin`),
  `/opt/php*/bin`; Windows `C:\php*`, `C:\tools\php*`.
- Dedupe by canonical path.

**Probe** — prefer `php -V` + `php -m` over parsing `php -i`: both are
stable, terse, and cover everything v1 needs. Reach for `php -i` /
`php --ini` only for the specific extra data those two don't give, and
only when actually needed.

- `php -V` → **version + flavor** from the first line, e.g.
  `PHP 8.3.12 (cli) (built: …) ( NTS )` / `( ZTS )` / `( NTS DEBUG )` /
  `( ZTS DEBUG )` → maps to bougie `Flavor` (nts / zts / nts-debug /
  zts-debug). (Architecture isn't needed — a system PHP that runs *is*
  the host arch.)
- `php -m` → **loaded extensions** (the `[PHP Modules]` / `[Zend
  Modules]` lists) — for `require.ext-*` validation. (Same data as
  `get_loaded_extensions()`, no `-r` script needed.)
- **Only when needed:**
  - `php --ini` → the system scan dir + loaded ini, used *only* if the
    shim has to set `PHP_INI_SCAN_DIR` (xdebug overlay; deferred ext
    install) so distro conf.d stays loaded.
  - `php -i` → `zend_module_api_no` / `zend_extension_api_no`, used
    *only* by the deferred opportunistic-ext path.

Two cheap process spawns (`-V`, `-m`) per candidate in the common case.
Cache probe results in `$BOUGIE_CACHE` keyed by `(path, mtime)` so repeat
syncs don't re-spawn `php`.

New module: `crates/bougie-php-discovery/` (or `bougie-installer::system`)
— `discover() -> Vec<PathBuf>`, `probe(&Path) -> Result<SystemPhp>`.
`SystemPhp { path, version, flavor, extensions, abi, scan_dir, ini }`.

## Selection

A selection layer **above** the existing `Backend` (which is
HTTP/blob-fetch-oriented and a poor fit for a local binary). Three
candidate *kinds*, gathered cheaply, then ordered per the preference:

- **managed-installed** — `bougie_fs::store::list_installed` (already on
  disk under `installs/`); reuses today's `pick_php`-style scan.
- **system** — `discover()` + `probe()`.
- **managed-download** — the `Backend` resolve (only if downloads on).

```
enum PhpSource { Managed(Selected /* installed or via backend */), System(SystemPhp) }
fn select_php(spec, flavor, required_exts, preference, downloads, paths) -> Result<PhpSource>
```

A candidate qualifies when its **version** satisfies `spec` and its
**flavor** matches; a **system** candidate must *additionally* already
load **every `required_exts`** entry (from the probe). The ordering is
the table above:

- `Managed` (default): managed-installed → system(qualifies) →
  managed-download.
- `OnlyManaged`: managed-installed → managed-download.
- `OnlySystem`: system(qualifies) only.

`downloads = false` removes the managed-download tier. A system PHP that
lacks a required extension is simply *not a qualifying candidate*, so
selection naturally continues to the next tier (managed-download under
the default) — the "fall back to managed" behavior, with no special
casing. ABI numbers aren't needed for *use* (only the deferred
ext-install-onto-system path).

If nothing qualifies (e.g. `--no-managed-php` and the only system PHP
lacks a required ext, or `--no-php-downloads` with no installed match and
no qualifying system PHP), error with the specific reason.

## Sync integration

In `sync::ensure_synced` (`crates/bougie/src/commands/sync.rs:478+`),
replace the unconditional `backend_for` + `install_php_with_backend` with
`select_php(...)`:

- **Managed source** → today's path (download/extract into
  `installs/<version>-<flavor>/`, replicate bundled conf.d, install
  required extensions).
- **System source** → no download, no install tree, no bundled conf.d,
  **no extension install** (selection already guaranteed every required
  `ext-*` is loaded; a system PHP lacking one was disqualified and we'd
  be on a managed source instead). Then:
  1. Write the resolved marker `version-flavor` (unchanged format) **and**
     a new `.bougie/state/resolved-php-path` holding the absolute binary
     path.
  2. Write shims as usual (`.bougie/bin/php` → the bougie binary;
     `Role::Php` resolves the real binary, see Shim).

`SyncResult`: report "using system PHP `<version>-<flavor>` at `<path>`"
vs "installed managed PHP …".

## State marker + shim

- **New file** `.bougie/state/resolved-php-path` (absolute php path),
  written only for a system source. Backward compatible: managed projects
  don't have it.
- `bougie-fs::state`: add `write/read_project_resolved_php_path`.
- **Shim** (`crates/bougie/src/shim.rs`, `Role::Php`/`Role::PhpFpm`):
  after `read_project_resolved`, check for `resolved-php-path`:
  - present → exec **that** binary;
  - absent → today's `installs/<version>-<flavor>/bin/php`.
- **`PHP_INI_SCAN_DIR` for system PHP:** overriding it would *disable*
  the distro's own conf.d. So for a system source, only set
  `PHP_INI_SCAN_DIR` when bougie actually has dirs to add (xdebug overlay
  in v1), and when set, **prepend the probed system scan dir** so distro
  extensions stay loaded. With nothing to add, leave it unset (system PHP
  behaves exactly as it does outside bougie).
- `php-fpm` from a system PHP: `Role::PhpFpm` resolves a sibling
  `php-fpm` next to the system `php` (probe records it); if absent, the
  server/`up` path errors clearly.

## Extensions on a system PHP — switch to managed (if allowed)

Installing a `.so` requires bougie's ABI-controlled toolchain, which a
foreign system build can't provide. So **needing a bougie-managed
extension forces a managed PHP** — but instead of erroring, bougie
*switches* the project to a managed PHP when the preference allows it
(i.e. not `--no-managed-php`). This is the load-bearing rule:

- **`bougie ext add <ext>`** on a system-PHP project: if managed PHP is
  allowed, transparently switch — resolve + install the matching managed
  PHP, re-point the resolved markers (drop `resolved-php-path`), then
  install the extension onto the managed PHP (today's flow). Print what
  happened ("switched to managed PHP 8.3.12 to install ext-redis"). Under
  `--no-managed-php`, error with guidance (relax the preference, or
  install the extension via the OS/PECL yourself).
- **`bougie sync`** with a declared `require.ext-*` the system PHP lacks:
  same rule — the system PHP is disqualified as a candidate (see
  Selection), so selection falls through to managed when allowed; under
  `--no-managed-php` it errors ("system PHP at `<path>` is missing
  required extension `ext-redis`; install it, or allow managed PHP").
- A system PHP is therefore only *used* when the project needs **no**
  bougie-managed extensions beyond what that PHP already loads.

`bougie ext list` still shows what a system PHP loads (from the probe).

**Deferred follow-up:** opportunistic install onto the *system* PHP —
when its `zend_module_api_no` exactly matches a bougie prebuilt's `abi`,
install the `.so` and enable it via a bougie conf.d the shim adds through
`PHP_INI_SCAN_DIR` (prepending the system scan dir), avoiding the switch.
Gated behind the exact ABI match; otherwise the switch-to-managed rule
above stands.

## CLI / config surface

- Flags on `sync` / `run` (and wherever PHP is resolved), mirroring uv:
  `--managed-php`, `--no-managed-php`, `--no-php-downloads`.
- `[php] managed` (`Option<bool>`) + `[php] downloads` (`Option<bool>`)
  config fields (`PhpConfig`), merged like `version` / `flavor`
  (`crates/bougie-config`).
- `bougie php list` / `bougie php find`: extend to also show *discovered
  system* PHPs (marked `system`), not just managed installs — uv's
  `uv python list` shows both. Reuse the discovery module.

## Phasing

1. ✅ **Discovery + probe + `SystemPhp` model** — new crate
   `bougie-php-discovery`: `discover()` (PATH + well-known dirs) +
   `probe()` parsing `php -V` (version/flavor, distro+RC suffixes
   stripped) and `php -m` (loaded extensions, `Zend `-prefix aware).
   Unit-tested per flavor + a real shell-stub probe. No sync wiring.
2. ✅ **Preference + selection** — `PhpPreference` enum
   (`OnlyManaged | Managed | OnlySystem`) resolved from
   `--managed-php`/`--no-managed-php` + `[php] managed`; pure `select`
   policy over `(managed-installed, system)` candidate sets with the
   ext-gate, in `bougie-php-discovery::select`. Shared
   `bougie_version::matches::version_satisfies` (deduped from
   `bougie-tool`). Unit-tested against synthetic candidate sets.
   (`--no-php-downloads` wiring lands with sync in Phase 3.)
3. ✅ **Sync + shim** — `ensure_synced_with(resolution)` gathers
   managed-installed + system candidates, runs `select`, and dispatches:
   system source writes `resolved` + `resolved-php-path` and shims (no
   download/install/ext); managed source clears the marker and runs the
   existing flow. Shim `Role::Php`/`PhpFpm` reads `resolved-php-path` and
   execs the system binary (php-fpm via sibling; no `PHP_INI_SCAN_DIR`
   overlay). CLI `--managed-php`/`--no-managed-php`/`--no-php-downloads`
   (flattened `PhpPrefArgs` on `sync`/`run`) + `[php] managed`/`downloads`
   config → `PhpResolution`. `SyncResult.php_source` (managed|system).
   Integration-tested with a fake `php` stub on PATH under
   `--no-managed-php` (selection, marker, shim exec, missing-ext error).
4. ✅ **ext switch-to-managed + php-list surfacing** — `bougie ext add`
   forces a managed PHP (switching off a system PHP, with a notice) when
   allowed; under an only-system preference (`--no-managed-php` /
   `[php] managed = false`) it errors with guidance. `ext remove` honors
   the project's natural (config) preference so it doesn't force a switch.
   `php list` shows discovered system PHPs under a `system` status (kept
   verbatim, not collapsed); `php find` falls back to a matching system
   PHP when no managed install matches. Integration-tested (list shows
   the stub; ext add `--no-managed-php` errors).
5. **(Deferred)** opportunistic ABI-matched extension install onto the
   system PHP (avoids the switch).

## Testing

- Probe: parse canned `php -V` (each flavor: NTS/ZTS/DEBUG) + `php -m`
  output; a couple of real-world layouts (Debian, Homebrew). Reach for
  `php --ini` / `php -i` only in the cases that need them.
- Discovery: temp dir on `PATH` holding a `php` shell stub that emits the
  probe output; assert it's found + probed.
- Selection: table tests over `(preference, downloads, candidates)` →
  expected source.
- Integration (`tests/`): fake `php` stub satisfying `require.php`;
  `bougie sync --no-managed-php` uses it, writes `resolved-php-path`, and
  `bougie run -- php -v` execs the stub; version/flavor mismatch →
  rejection; missing required ext → guided error; `--no-php-downloads`
  with no system PHP → clear error; `bougie ext add` on a system-PHP
  project → switches to managed (or errors under `--no-managed-php`).

## Open questions

- **`php-fpm`/server with system PHP.** v1 supports it if a sibling
  `php-fpm` exists next to the system `php`; otherwise server features
  need managed PHP. OK to ship with that limitation?
- **Windows.** Discovery locations + `php -V`/`-m` parsing are
  cross-platform; confirm we want system-PHP discovery on Windows in v1
  (the managed path there is `windows.php.net`).
