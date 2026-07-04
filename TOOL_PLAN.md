# bougie tool — implementation plan

Working plan for shipping `bougie tool`: globally-installed, isolated PHP
CLI tools (phpstan, psalm, php-cs-fixer, rector, deployer, …), each
pinned to its own PHP version and composer-resolved dependency set.

Models uv's `uv tool` UX onto PHP/Composer primitives. The interesting
mechanical bits — bookkeeping, shim resolution, wrapper format — diverge
from uv only where PHP forces them to.

Phases are bottom-up; stop at any phase and the preceding work still has
value.

## Mental model

A **tool** is a Composer-installable package whose `bin` entries are
exposed globally. Each tool gets its own `composer.json` + `vendor/`
tree under `$BOUGIE_HOME/tools/<vendor>-<name>/`, independent of any
project. Users think "I installed phpstan"; the per-tool vendor dir,
the receipt, and the PATH shim are how that illusion holds up.

Bougie's two pre-existing surfaces stay cleanly separated from this
one:

| Invocation                | PHP used            | conf.d source         | Project-aware? |
| ---                       | ---                 | ---                   | ---            |
| `phpstan` (global tool)   | tool's pinned PHP   | `$TOOL_DIR/conf.d/`   | No             |
| `bougie run phpstan`      | project's pinned PHP| `<project>/vendor/bougie/`  | Yes            |
| `bougie run phpunit`      | project's pinned PHP| `<project>/vendor/bougie/`  | Yes            |

`bougie run` opts into project context. `bougie tool` opts out of it.

## Surface (CLI)

```
bougie tool install <vendor/name>[@<constraint>] [--with <vendor/name>...]
                                                 [--php <ver>]
                                                 [--composer <ver>]
                                                 [--force]
bougie tool run [--no-project] <vendor/name>[@<constraint>] [args...]   # also: bgx <vendor/name> [args...]
bougie tool list [--installed | --available]
bougie tool upgrade <vendor/name> | --all [--reinstall]
bougie tool uninstall <vendor/name>
bougie tool inject <vendor/name> <pkg-or-ext>...         # add composer plugin or PHP extension
bougie tool uninject <vendor/name> <pkg-or-ext>...
bougie tool dir [<vendor/name>]
bougie tool update-shell                                 # ensure bin dir is on PATH
```

`<vendor/name>` is always the full Composer identifier. **No alias
table** — short names are the user's shell-alias concern.

## Pre-existing bougie code we lean on

- `src/cli.rs` — clap derive entry. New `ToolCommand` enum follows the
  shape of `ServerCommand` / `ComposerCommand`.
- `src/paths.rs` — `BOUGIE_HOME` / `BOUGIE_CACHE` resolution. Add
  `tools()`, `tool_dir(name)`, `tool_bin_dir()`, `tool_launcher_dir()`.
- `src/store.rs` — install-path helpers; receipts and wrappers follow
  the same `<name>-<version>` directory convention.
- `src/composer/` — phar fetch + composer-the-tool management. Reused
  verbatim for the per-tool composer run.
- `src/composer/resolve.rs`, `src/composer/lockfile.rs` — composer
  request resolution and lockfile reading. Reused per-tool.
- `src/shim.rs` — argv[0]-dispatched exec path. We extend it with a new
  `Role::ToolExec` (Unix) and add an explicit `tool-exec` subcommand
  surface (Windows + parity).
- `src/conf_d.rs` — conf.d scanning. The same shape applies to
  `$TOOL_DIR/conf.d/`.
- `src/lock.rs::ExclusiveGuard` — per-tool exclusive locks during
  install/upgrade/uninstall.
- `src/index/` — PHP extension index, used by `bougie tool inject` when
  a tool needs an extension built against its pinned PHP.

## On-disk layout

Cross-platform paths. `$BOUGIE_HOME` resolves via the existing
XDG-on-every-platform rules (`paths.rs`).

```
$BOUGIE_HOME/
  bin/
    bougie                          # stable symlink (Unix) / copy (Windows) to the active bougie binary
                                    # rewritten by `bougie self update`
  tools/
    <vendor>-<name>/
      receipt.toml                  # bookkeeping; see "Bookkeeping" below
      composer.json                 # generated; the package + any --with extras
      composer.lock
      vendor/                       # populated by composer install
      bin/
        <binname>                   # per-entry-point wrapper (PHP file with bougie shebang on Unix;
                                    # also exists on Windows for parity, invoked via .cmd in bin dir)
      conf.d/                       # PHP_INI_SCAN_DIR target; empty by default

$BOUGIE_TOOL_BIN_DIR/               # user PATH dir
  <binname>                         # Unix: symlink to $TOOL_DIR/bin/<binname>
  <binname>.cmd                     # Windows: small .cmd wrapper (see "Wrappers — Windows")
```

Defaults for `$BOUGIE_TOOL_BIN_DIR`:

- Unix: `$XDG_BIN_HOME` if set, else `~/.local/bin`.
- Windows: `%LOCALAPPDATA%\bougie\bin`.
- Env override `BOUGIE_TOOL_BIN_DIR` on both platforms.

## Bookkeeping — `receipt.toml`

Per-tool manifest, written at install, mutated on `inject` / `upgrade`,
deleted on `uninstall`. Modelled on uv's `uv-receipt.toml`.

```toml
package = "phpstan/phpstan"
constraint = "^1.10"
php_version = "8.3.12"
php_flavor = "nts"
composer_version = "2.7.1"
with = ["phpstan/phpstan-strict-rules"]

# Denormalized for the hot path (Role::ToolExec reads this and execs).
# Refreshed on `bougie php upgrade` of the pinned interpreter.
php_resolved_path = "/Users/jelle/.local/share/bougie/installs/8.3.12-nts/bin/php"

[[entrypoints]]
name = "phpstan"
install_path = "/Users/jelle/.local/bin/phpstan"      # what `uninstall` deletes
from = "phpstan/phpstan"
```

**Rules** (all copied straight from uv's model — it works):

- `bougie tool uninstall` reads `entrypoints[*].install_path` and
  removes those files. It does **not** scan the bin dir for "files we
  own"; there is no global manifest.
- `bougie tool install` precomputes target paths and bails if any
  exist (`executable already exists at <path> (use --force to
  overwrite)`). This naturally protects user-placed bins and
  `vendor/bin` exports that might share the bin dir.
- `bougie tool list` walks `tools/*/receipt.toml`. Missing,
  unparseable, or stale receipts (pinned PHP no longer installed) are
  surfaced as orphans. **Orphan bin-dir files are not auto-cleaned** —
  without a receipt we have no record of what to delete. Recovery is
  `bougie tool install --force <pkg>` or manual.

## Shim mechanism

The shim's only job is: read the wrapper path, find the tool dir, read
the receipt, set `PHP_INI_SCAN_DIR`, exec the pinned PHP with the
wrapper script as argv. It is shared between platforms — only how it
gets invoked differs.

A new `tool-exec` subcommand on the bougie binary implements this:

```
bougie tool-exec <wrapper-path> [user-args...]
    tool_dir = wrapper-path.parent().parent()
    receipt  = read tool_dir/receipt.toml
    env PHP_INI_SCAN_DIR=tool_dir/conf.d \
        BOUGIE_TOOL=receipt.package \
    exec receipt.php_resolved_path <wrapper-path> <user-args...>
```

The wrapper path's filesystem location *is* the address of the tool
dir — no env lookup, no CWD walking, no central manifest needed. The
wrapper is always at `$TOOL_DIR/bin/<binname>`.

`tool-exec` errors (missing receipt, missing PHP, malformed wrapper)
report with the tool name pulled from the wrapper basename, with a
`bougie: tool <name> is broken — run 'bougie tool upgrade --reinstall'`
recovery hint.

## Wrappers — Unix

The wrapper is a PHP file with a bougie-binary shebang:

```php
#!/Users/jelle/.local/share/bougie/bin/bougie tool-exec
<?php
// Generated by bougie tool. Regenerate with
// `bougie tool upgrade --reinstall phpstan/phpstan`.

$argv[0] = $_SERVER['argv'][0] = 'phpstan';
$GLOBALS['_composer_autoload_path'] = __DIR__ . '/../vendor/autoload.php';
require __DIR__ . '/../vendor/phpstan/phpstan/bin/phpstan';
```

What each piece does:

- Shebang points at `$BOUGIE_HOME/bin/bougie` (the **stable** symlink,
  not the user's original install path). Survives bougie self-update.
- Kernel passes the wrapper path as argv[1]; bougie's `tool-exec`
  derives the tool dir from it.
- `$argv[0]` fix-up gives clean error messages (tools that print
  "Usage: phpstan …" rather than "Usage: /full/path/to/script").
- `$GLOBALS['_composer_autoload_path']` declares the wrapper a
  Composer-style bin proxy (the composer-runtime-api convention that
  Composer ≥2.2's own `vendor/bin` proxies set before including the
  real bin). Entry points with "was I included or invoked?" guards —
  psysh's `Psy\Shell::isIncluded` is the canonical case — tolerate
  exactly one include frame when the global is set; without it they
  silently bail under any require-style launcher. The value is
  truthful: it points at the tool's vendor autoloader.
- `__DIR__`-relative `require` — no `$TOOL_DIR` baked into the
  wrapper, so the wrapper survives a future `$BOUGIE_HOME` move.

PATH entry: `~/.local/bin/phpstan -> $TOOL_DIR/bin/phpstan` (symlink).
The wrapper file lives in the tool dir; `uninstall` removes both.

## Wrappers — Windows

No shebangs, unreliable symlinks. Two files:

**`$TOOL_DIR/bin/phpstan.php`** — identical to Unix, minus the shebang
(the file is never directly executed):

```php
<?php
// Generated by bougie tool. Regenerate with
// `bougie tool upgrade --reinstall phpstan/phpstan`.

$argv[0] = $_SERVER['argv'][0] = 'phpstan';
$GLOBALS['_composer_autoload_path'] = __DIR__ . '/../vendor/autoload.php';
require __DIR__ . '/../vendor/phpstan/phpstan/bin/phpstan';
```

**`$BOUGIE_TOOL_BIN_DIR\phpstan.cmd`** — small launcher copied (not
symlinked) into the PATH dir:

```cmd
@echo off
"C:\Users\jelle\AppData\Local\bougie\bin\bougie.exe" tool-exec ^
  "C:\Users\jelle\AppData\Local\bougie\tools\phpstan-phpstan\bin\phpstan.php" %*
```

Bin extension is `.cmd` because `PATHEXT` includes `.CMD` by default,
so `phpstan` resolves in both cmd.exe and PowerShell. Absolute paths
are baked in; `bougie self update` rewrites them on bougie-binary
relocation (same trigger as the Unix symlink rewrite).

PowerShell: `.cmd` works. Git Bash / MSYS: not supported in v1 — those
users should install the WSL/Linux build.

`tool-exec` on Windows `CreateProcess`'s `php.exe` and waits for it,
propagating exit code and Ctrl+C / Ctrl+Break.

## Why a shim at all, vs uv's absolute-path shebang

uv bakes the absolute path to the tool's venv's `python` into the
wrapper's shebang. If that interpreter is GC'd, every shim breaks. We
add one level of indirection so:

- `bougie php upgrade 8.3.12 → 8.3.13` (same major) rewrites
  `php_resolved_path` in each affected receipt; wrappers keep working.
- `bougie self update` rewrites the stable bougie symlink (Unix) or
  the `.cmd` wrapper paths (Windows); shebangs/launchers keep
  working.
- `PHP_INI_SCAN_DIR` and other env can be set *before* PHP starts,
  which a pure-shebang approach can't do (shebangs accept one arg, no
  env). This is what makes per-tool extension scoping possible
  (below).

The cost is two execs (bougie → php) instead of one. ~1ms; negligible
for tools that take hundreds of ms to start.

## Per-tool extensions (the actual point of the conf.d dir)

`$TOOL_DIR/conf.d/` is empty by default. The shim sets
`PHP_INI_SCAN_DIR` unconditionally — at zero cost, since we're going
through `tool-exec` anyway. So extensions are scoped per tool without
any extra plumbing.

`bougie tool inject phpstan/phpstan intl` then:

1. Ensures `intl.so` (or `.dll`) exists in the shared extension store
   for the tool's pinned `(php_version, flavor)`. Compiles via the
   existing `bougie ext add` machinery if not.
2. Writes `$TOOL_DIR/conf.d/20-intl.ini` containing `extension=intl`.

The `.so` is shared per-PHP-build (no duplication across tools or
projects); the **load decision** is per-tool. Same partition projects
already use.

`bougie tool inject phpstan/phpstan phpstan/phpstan-phpunit` (a
Composer plugin, not an extension) rewrites `composer.json`,
`composer update`s, refreshes the receipt. The plugin's bin entries,
if any, are *not* exposed — only the parent tool's bins are.

Bougie distinguishes "extension" vs "composer package" by looking up
the name in the extension index first; ambiguous names error with a
hint.

## Composer plugin permissions

Generated `composer.json` sets `allow-plugins` narrowly — only the
specific plugin packages the user added via `--with` / `inject` are
allowed, never `"*"`. Composer 2.x requires this for any plugin to
load.

## Locking

Per-tool exclusive lock at `$TOOL_DIR/.lock` for install / upgrade /
inject / uninstall, using `ExclusiveGuard` with a 1-minute timeout
(matches `src/composer/mod.rs`).

Global lock (`paths.global_lock()`) only when mutating the shared
extension store via `bougie ext add` triggered by `tool inject`.

The bin-dir writes (symlink / .cmd creation) need no lock: the
`path.exists()` precheck plus atomic `rename`-then-link gives "first
writer wins" semantics, which is the correct behavior for collisions
anyway.

## Ephemeral runs (`bougie tool run`, `bgx`)

One-shots without persisting an install. Cache key:
`(package, constraint, php_version, composer extras, extension set)`.
Cache root: `paths.cache().join("tool-run").join(<hash>)`.

If a persistent install matches the request exactly (including the
extension set), reuse it. Otherwise materialize into the cache and
run. GC: existing `bougie cache prune` walks tool-run entries by
mtime, drops anything older than the configured TTL.

### Requirement-derived extensions (shipped)

The tool's own `require.ext-*` (from the same Packagist metadata fetch
that supplies `require.php`) joins the effective extension set as
implicit `--with`s, minus names that are builtin or baseline.
Derived names are *soft*: one bougie's index can't satisfy warn-skips
instead of failing the run. Explicit `--with` names stay hard errors.

### Project context (shipped; ephemeral lane only)

Tools like n98-magerun2 and deployer are project *clients* — they boot
the surrounding application but can't live in its `composer.json`
(dependency conflicts are why `-dist` repacks exist). So `tool run` /
`bgx`, unless `--no-project` is passed, derives context from the
nearest ancestor with `composer.json` / `bougie.toml` /
`vendor/bougie/` (`commands/tool_project.rs`):

- **PHP** (`resolve::select_php` ladder): `--php` wins; else a synced
  bougie project's exact resolved interpreter when it satisfies the
  tool's `require.php`; else the highest install matching project ∩
  tool constraints (auto-install by the project's written constraint,
  kept only if the tool accepts the result); else the tool's own
  `require.php` with a warning — the tool must at minimum run itself,
  and the project's `platform_check.php` reports any residual
  mismatch in its own words. Project constraint mirrors sync's
  precedence: `bougie.toml [php]version` ∩ `composer.json
  require.php`, falling back to `infer_php` (Magento matrix, then
  lockfile intersection).
- **Extensions**: `composer.json require.ext-*` ∪ inferred
  (framework recommended set + lockfile `ext-*`), minus
  builtin/baseline, minus `[extensions] name = false` opt-outs.
  Same soft warn-skip policy as requirement-derived names.
- A one-line stderr notice states what was applied and points at
  `--no-project`.

The derived set is part of the cache key, so the same tool run from
two differently-shaped projects gets two slots. `tool install` stays
project-blind — a global tool behaves identically from any cwd.

### CLI ini defaults (shipped)

`tool-exec` layers `$BOUGIE_LOCAL/cli-defaults/` (lazily written,
currently just `memory_limit = -1`) into `PHP_INI_SCAN_DIR` between
the install's conf.d and the tool's conf.d, so a per-tool fragment
still wins. An ini fragment rather than a `-d` argv flag because the
scan dir travels via the environment: child PHP processes a tool
spawns (n98-magerun2 running `PHP_BINARY bin/magento`) inherit it,
which argv flags never reach. Mirrors the project-side `php` shim's
`-d memory_limit=-1` stance; FPM is unaffected.

### Guarded bins (`isIncluded` et al.) — handled

Require-style wrappers trip "am I the main script?" guards; psysh's
`bin/psysh` (`Psy\Shell::isIncluded(debug_backtrace())`) silently
no-ops under a bare `require`. Solved by speaking Composer's bin-proxy
convention: the wrapper sets `$GLOBALS['_composer_autoload_path']`
before the require (see "Wrappers — Unix"), which convention-aware
guards accept as one legitimate include frame. Wrappers regenerate on
install / upgrade / reinstall; a pre-fix wrapper in an old cache slot
regenerates when the slot is pruned or its cache key changes.

### `bgx` — short alias binary

`bgx` is to `bougie tool run` what `uvx` is to `uv tool run`: a
separate binary that prepends `["tool", "run"]` to its argv and
dispatches into the same library. Three chars, follows the
`uvx`/`npx`/`pipx` pattern, low collision surface (two-char `bx` is
risky — there's an existing Ruby `bx` wrapper in the wild).

Shipped as a second `[[bin]]` in `Cargo.toml` rather than an
argv[0]-symlink to `bougie`:

```toml
[[bin]]
name = "bgx"
path = "src/bin/bgx.rs"
```

```rust
fn main() -> std::process::ExitCode {
    bougie::run_with_prefix(&["tool", "run"])
}
```

Reasons to ship a real binary instead of symlinking (same reasoning
uv uses for `uvx`):

- **Windows parity.** No reliable user-mode symlinks; we'd need two
  files on Windows anyway. One mechanism on all platforms keeps the
  installer simple.
- **Self-update atomicity.** `bougie self update` replaces each
  binary independently; no "what if the symlink got replaced by the
  user / by a packager" edge cases.
- **macOS argv[0] quirks.** Some exec paths canonicalize symlinks
  before invoking, so a symlinked `bgx` could arrive with
  `argv[0] == "bougie"` and silently mis-dispatch. A real binary
  avoids it.
- **Cost is negligible.** The second binary is ~5 lines; all real
  code lives in the bougie library crate. Disk overhead is the size
  of a stripped Rust launcher (tens of KB), not a duplicated bougie.

No general-purpose `b` / `bx` alias for the whole bougie CLI —
`bougie` is short enough, and an alias for every subcommand fragments
the brand without solving a real ergonomic problem. Users who want it
can `alias b=bougie` in their shell rc.

## Module layout (`src/tool/` + `src/commands/`)

New `src/tool/` mirrors `src/composer/`:

```
src/tool/
  mod.rs            # public surface; install/upgrade/uninstall/inject/list/run
  request.rs        # parse <vendor/name>[@constraint] + --with + --php
  resolve.rs        # composer-side resolution → concrete versions
  install.rs        # generate composer.json, run composer, write wrappers + symlinks/.cmd
  uninstall.rs      # receipt-driven file removal
  upgrade.rs        # re-resolve, swap atomically
  inject.rs         # add composer plugin or PHP extension to an installed tool
  receipt.rs        # ToolReceipt struct + (de)serialise
  wrapper.rs        # platform-specific wrapper + launcher emission
  exec.rs           # tool-exec subcommand handler (read receipt, set env, exec php)
  run.rs            # ephemeral tool run (bgx)
```

`src/commands/`:

```
tool_install.rs, tool_run.rs, tool_list.rs, tool_upgrade.rs,
tool_uninstall.rs, tool_inject.rs, tool_uninject.rs, tool_dir.rs,
tool_update_shell.rs, tool_exec.rs
```

`src/cli.rs`: new `Command::Tool(ToolCommand)` and a hidden
`Command::ToolExec { wrapper: PathBuf, args: Vec<OsString> }` for the
shim entry. `tool-exec` is hidden from `--help` but documented in
CLI.md.

`src/shim.rs`: no new `Role` variant strictly required (we use the
explicit subcommand on both platforms). Optionally add `Role::ToolExec`
that maps the stable `$BOUGIE_HOME/bin/bougie` symlink invocation to
the same handler when argv[1] looks like a wrapper path — keeps Unix
shebang invocations from going through clap.

## Phasing

Each phase ends in a runnable/shippable state.

### Phase 1 — persistent install/uninstall, Unix only

- `paths.rs` additions: `tools()`, `tool_dir(name)`, `tool_bin_dir()`,
  `tool_launcher_dir()`.
- `$BOUGIE_HOME/bin/bougie` stable symlink, created/refreshed by
  `bougie self install` and `bougie self update`.
- `src/tool/receipt.rs` + `src/tool/wrapper.rs` (Unix only).
- `bougie tool install <vendor/name>[@<constraint>]` — single bin,
  no `--with`, no `--php` flag (latest matching PHP picked).
- `bougie tool uninstall`, `bougie tool list`, `bougie tool dir`.
- `bougie tool-exec` subcommand (the runtime shim).
- Collision check on bin-dir write.

Shippable: install ~6 common tools by full vendor/name, run them from
PATH.

### Phase 2 — flags, plugins, extensions

- `--with <pkg>` and `bougie tool inject` / `uninject`.
- `--php <ver>` and `--composer <ver>` pinning.
- Extension injection (calls into existing `bougie ext add` for the
  tool's pinned PHP).
- `bougie tool upgrade <pkg>` + `--all` + `--reinstall`.
- Multi-bin tools (a single package exposing several entry points).

### Phase 3 — ephemeral runs + `bgx`

- `bougie tool run`.
- New `[[bin]] bgx` shipped alongside `bougie`, prepending
  `["tool", "run"]` to argv.
- Cache layout under `paths.cache()/tool-run/`.
- `bougie cache prune` integration.
- `bougie self install` / `self update` installs and refreshes the
  `bgx` binary alongside `bougie`.

### Phase 4 — Windows

- Windows wrapper emission (`.cmd` launcher in bin dir, `.php` wrapper
  in tool dir).
- `tool-exec` Windows path: `CreateProcess` + wait, propagate exit code
  and Ctrl+C.
- Default `$BOUGIE_TOOL_BIN_DIR` = `%LOCALAPPDATA%\bougie\bin`.
- `bougie self update` rewrites both the bougie copy at
  `$BOUGIE_HOME\bin\bougie.exe` and the absolute paths in every
  `*.cmd` launcher in the bin dir.

### Phase 5 — PATH ergonomics

- `bougie tool update-shell` — appends bin dir to user shell rc files
  (bash / zsh / fish on Unix; user PATH env on Windows via the same
  registry-write path uv uses).
- Detect-and-warn during `bougie tool install` if the bin dir isn't on
  PATH.

## Receipt-driven failure modes

The receipt is load-bearing at runtime, so the shim's failure paths
need clean messages:

| Failure                              | tool-exec output                                                                       |
| ---                                  | ---                                                                                    |
| Wrapper deleted                      | (Never reached — kernel/OS error on the missing file before tool-exec is invoked.)     |
| Wrapper path outside `$BOUGIE_HOME/tools` | `bougie: refusing to tool-exec a wrapper not under $BOUGIE_HOME/tools`              |
| Receipt missing                      | `bougie: tool <name> is broken — run 'bougie tool upgrade --reinstall <vendor/name>'`  |
| Receipt parse error                  | Same hint, but include `(receipt corrupt: <err>)`.                                      |
| `php_resolved_path` no longer exists | `bougie: tool <name> pinned to PHP <ver> which is no longer installed (...)`           |

## Interaction with existing surfaces

- **`bougie run`** is unchanged. Inside a project, it goes through
  `vendor/bougie/bin/php`, not `tool-exec`. The two shim paths share zero
  code at runtime, only the `bougie` binary itself.
- **`bougie services` / `bougied`** unaffected — tools don't talk to
  the supervisor.
- **`bougie ext add`** gets called *by* `bougie tool inject` when a
  tool needs an extension built. Otherwise unchanged.
- **`bougie composer`** unchanged. The composer used inside a tool dir
  is the user's tool-pinned composer.phar, fetched via the existing
  `composer/fetch.rs` path.
- **`bougie self update`** rewrites `$BOUGIE_HOME/bin/bougie`
  (symlink Unix, copy Windows) and on Windows also rewrites every
  `*.cmd` launcher in `$BOUGIE_TOOL_BIN_DIR`. New responsibility for
  self-update; small.

## Open questions to settle before phase 1

- **Bin name collision policy**: hard-error + `--force`, matching uv.
  Confirm there's no desire for "rename on collision" or "prefix with
  tool name".
- **`bougie tool list --available`**: scope of the package index it
  queries. Packagist is the obvious answer; do we want to filter to
  "has `bin` entries"? Defer to phase 2.
- **Windows `bougie self update` rewriting `.cmd` launchers**: simpler
  alternative is to make the `.cmd` call a fixed alias like
  `%BOUGIE_HOME%\bin\bougie.exe`, requiring `BOUGIE_HOME` to be set in
  the user's env. uv does the latter for portability. Lean toward
  absolute paths + rewrite, since it doesn't require env setup.
- **`receipt.php_resolved_path` staleness**: who refreshes it? Cleanest
  is `bougie php upgrade` walking every receipt that pinned the
  upgraded version. Add to `commands/php_upgrade.rs` as a final
  fixup step.

## Out of scope

- Aliases / short names. Always full `vendor/name`.
- `--editable` tool installs. Tools are released artifacts.
- Tool autocompletion injection into shells. Per-tool concern; not
  ours to centralize.
- Cross-tool dependency dedup (sharing vendor dirs between tools).
  Disk savings don't justify the complexity; each tool gets its own
  isolated tree.
- WSL / Git Bash / MSYS as first-class Windows targets. Use the
  Linux build under WSL.
- A `bougiew.exe` Windows-GUI-subsystem variant (the equivalent of
  uv's `uvw.exe`). PHP CLI tools we target are strictly console; revisit
  if a GUI use case emerges.
- Project-overlaid `PHP_INI_SCAN_DIR` chains that layer the project's
  own conf.d verbatim onto a tool run. Superseded by the shipped
  project-context design (see "Project context" above), which derives
  the extension *set* from the project and materialises fragments in
  the tool slot for the tool's PHP — reproducible per cache key, no
  cross-interpreter `.so` reuse.
