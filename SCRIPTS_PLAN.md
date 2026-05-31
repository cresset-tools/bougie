# SCRIPTS_PLAN — opt-in root `composer.json` script execution

Status: design. Not started. Opt-in / off by default (see TODO.md entry,
decided 2026-05-30). This doc covers **how the event system would work**;
the *whether/when* is settled (opt-in).

**Scope (decided 2026-05-31): non-internal scripts only.** bougie runs the
process-spawning entry forms (`@php`, `@composer`, `@putenv`, `@<alias>`,
shell). Scripts that reach into Composer internals via a PHP callback
(`Class::method` + the `Event` object → `IO`, `Composer`, `Config`, …) are
an explicit non-goal — that's an antipattern the ecosystem already
abandoned for plugins (see "PHP callbacks / Composer internals" below).

## Why this is safe (recap)

Composer only ever runs `scripts` from the **root** package, never from
dependencies. So running them is not a supply-chain hazard — they're the
project author's own commands. bougie keeps it **opt-in / off by default**:
deterministic native behavior stays the default, and a freshly-cloned
untrusted repo's `post-install-cmd` must not auto-run on `bougie sync`
(this is why Composer ships `--no-scripts`).

Plugins (`composer/installers`, `magento/magento-composer-installer`,
Symfony Flex) are *not* scripts and are unaffected — they stay native.

## Composer's model (what we're emulating)

A script **event** has an ordered list of **listener entries** in root
`scripts.<event>`. On dispatch, Composer runs each entry in order; a
non-zero exit aborts the event (and the command). Entry kinds:

| Entry form | Meaning |
|---|---|
| `@php <args>` | run with Composer's PHP binary |
| `@composer <args>` | re-invoke Composer itself |
| `@putenv KEY=VAL` | set env var for *subsequent* entries in this dispatch |
| `@<name>` | **script alias** — recurse into `scripts.<name>` |
| `Vendor\Class::method` | **PHP callback** — called in-process with a `Composer\Script\Event` |
| anything else | shell command via `ProcessExecutor` |

Composer also: prepends the bin-dir (`vendor/bin`) to `PATH`, sets
`COMPOSER_DEV_MODE`, `COMPOSER_BINARY`, `COMPOSER`, `COMPOSER_RUNTIME_ENV`;
applies a process timeout (default 300s) that
`Composer\Config::disableProcessTimeout` (a callback) turns off; runs in
the project root.

### Command events, in dispatch order

- **install** (`bougie sync` / `bougie composer install`):
  `pre-install-cmd` → (package ops) → `pre-autoload-dump` → *dump* →
  `post-autoload-dump` → `post-install-cmd`
- **update** (`bougie composer update`): same with
  `pre-update-cmd`/`post-update-cmd`.
- Per-package events (`pre/post-package-{install,update,uninstall}`) exist
  but are largely plugin territory; **deferred** (see MVP).

## bougie architecture

### New crate: `bougie-scripts`

Pure parse + classify + dispatch, no FS-orchestration knowledge. Mirrors
how `bougie-installers` isolates declarative-plugin logic. Reusable by the
resolver's install path, a future `bougie composer run-script`, and
`bougie-recipe`.

```rust
/// Parsed root `scripts` table: event/alias name → ordered entries.
pub struct Scripts { /* IndexMap<String, Vec<Entry>> (preserve order) */ }
impl Scripts { pub fn parse(root_composer_json: &Value) -> Self; }

pub enum Entry {
    Shell(String),
    Php(String),        // args after `@php`
    Composer(String),   // args after `@composer`
    PutEnv { key: String, val: String },
    Alias(String),      // `@name`
    Callback { class: String, method: String },
}

/// Everything dispatch needs from the host, injected by the caller so the
/// crate stays FS/PHP-agnostic and testable.
pub struct ScriptContext<'a> {
    pub project_root: &'a Path,
    pub php_bin: &'a Path,          // bougie's resolved PHP for this project
    pub bin_dir: &'a Path,          // vendor/bin (or config.bin-dir)
    pub base_env: Vec<(String,String)>, // COMPOSER_DEV_MODE, etc.
    pub dev_mode: bool,
    pub timeout: Option<Duration>,  // None = disabled
    /// Native handlers for callbacks bougie reproduces (e.g. Laravel's
    /// ComposerScripts::postAutoloadDump → clearCompiled). Keyed by
    /// "Class::method". Returning Handled skips the warn-and-skip path.
    pub callback_handlers: &'a CallbackRegistry,
}

pub enum EntryOutcome { Ran, SkippedCallback(String) /* warn */, NativeCallback }

pub fn dispatch(scripts: &Scripts, event: &str, ctx: &ScriptContext)
    -> Result<Vec<EntryOutcome>>;   // Err on non-zero exit / cycle
```

### Dispatch algorithm

```
dispatch(event, ctx, seen):
    if event in seen: return Err(cycle)
    seen += event
    env := ctx.base_env + PATH(bin_dir prepended)
    outcomes := []
    for entry in scripts[event]:
        match entry:
            PutEnv{k,v}      -> env[k] = expand(v, env)
            Alias(name)      -> outcomes += dispatch(name, ctx, seen)   // recurse
            Php(args)        -> run(ctx.php_bin, split(args), env, project_root)
            Composer(args)   -> run_composer_equivalent(args)           // see below
            Shell(cmd)       -> run_shell(cmd, env, project_root)       // sh -c / cmd /C
            Callback{c,m}    -> if ctx.callback_handlers.has(c,m) { handler.run(); NativeCallback }
                                else { warn("can't run PHP callback c::m"); SkippedCallback }
        if last run exited non-zero: return Err(abort)   // matches Composer
    outcomes
```

Cycle guard on alias recursion. `@putenv` mutates the dispatch-local env
only. Output streams straight through to the user (scripts are chatty).

### Integration: where events fire (resolver orchestrator)

In `install_from_lock` (and the update path), gated on opt-in:

```
if scripts_on { dispatch("pre-install-cmd"/"pre-update-cmd")?; }
... native resolve + extract + plugin deploys (unchanged) ...
if scripts_on { dispatch("pre-autoload-dump")?; }
dump_autoload();                                  // native, always
if scripts_on {
    dispatch("post-autoload-dump")?;              // runs user's package:discover etc.
} else {
    native_laravel_discovery();                   // current PR behavior
}
if scripts_on { dispatch("post-install-cmd"/"post-update-cmd")?; }
```

## Reconciling with the native reimplementations (PR #248)

This is the crux. The model: **native reproductions are the scripts-off
default; scripts-on mode runs the real entries and falls back to native
only for callback gaps.**

- **Laravel.** `post-autoload-dump` is one callback + one shell entry:
  - scripts-**off** (default): orchestrator runs `native_laravel_discovery`
    (build `packages.php` + `clearCompiled`) + the positional drift guard —
    exactly today's behavior.
  - scripts-**on**: `dispatch("post-autoload-dump")` runs the entries in
    order. The `ComposerScripts::postAutoloadDump` callback is registered
    in `callback_handlers` → bougie runs native `clearCompiled` (it can't
    invoke the PHP callback). The `@php artisan package:discover` shell
    entry runs for real → Laravel writes `packages.php` itself. **No
    double-write**, and the drift guard is moot (we're running whatever's
    declared).
  - So the native Laravel code becomes a **callback handler**, not a
    separate branch. The drift guard only matters in scripts-off mode.
- **Plugins (installers / magento-installer / Flex):** untouched by
  scripts mode — still native, always.

## Opt-in surface

- `bougie.toml`: `[scripts] run = true` (project-committed opt-in) — the
  author who trusts their own scripts turns it on for the repo.
- CLI: `--scripts` to enable for one run; `--no-scripts` to force-disable
  (Composer-compatible, and the escape hatch once/if the default ever
  flips). Precedence: CLI flag > bougie.toml > default(off).
- A future `bougie composer run-script <name>` (and maybe `bougie run
  <name>`) reuses `dispatch` for an arbitrary named script.

## Execution details

- **PHP binary:** bougie's resolved project PHP. `@php` → that binary; this
  is a genuine advantage (right PHP version, no `which php` ambiguity).
- **`@composer`:** bougie isn't Composer. Options, in order of preference:
  1. map the common subcommands to bougie equivalents (`@composer install`
     → no-op/we're mid-install; `@composer dump-autoload` → native dump);
  2. else run the composer escape-hatch tool (`bougie tool composer …`,
     once that exists); 3. else error with a clear message. MVP: (1) for
     `dump-autoload`/`install`, else warn-skip.
- **bin-dir on PATH:** prepend `vendor/bin` (honor `config.bin-dir`) so
  scripts find installed CLIs (`phpunit`, `pint`, …).
- **Env:** set `COMPOSER_DEV_MODE` (1/0), `COMPOSER_BINARY` (path to the
  bougie binary, for `@composer` self-calls), project-relevant `COMPOSER_*`.
  `@putenv` accumulates within the dispatch.
- **Failure semantics:** non-zero exit aborts the event and fails the
  command — same as Composer. (No partial "best effort".)
- **Timeout:** default 300s per process, matching Composer; disabled when
  the script contains the `disableProcessTimeout` callback (recognize it as
  a no-op callback handler) or via config.
- **Sandboxing:** root scripts run **unsandboxed** by default — they need
  DB sockets, network, fs. This is a deliberate carve-out from
  sandbox-by-default (which targets *spawned services*, not user-invoked
  trusted scripts). A future opt-in sandbox profile is possible but most
  real scripts (migrate, asset build) would fight it.
- **Cross-platform:** `@php`/`@composer`/`@putenv`/`@alias` are portable.
  Shell entries use `sh -c` on Unix, `cmd /C` on Windows (Composer's
  approach). Many real shell entries are Unix-only — the user's concern,
  not ours; we run them faithfully on the platform.

## PHP callbacks / Composer internals — explicit non-goal

`Vendor\Class::method` entries are invoked **in-process** by Composer with
a `Composer\Script\Event`, through which they reach into Composer's
internals — `IO` (including interactive prompts), the `Composer` object,
`Config`, `RootPackage`, the installed repository, the operation list.

**Decision: bougie does not support callbacks that use Composer
internals, and won't build the PHP-shim machinery to.** Rationale:

- **It's an antipattern that the ecosystem already abandoned.** The known
  offenders are all legacy and have migrated their logic into *plugins*
  (which bougie doesn't run regardless):
  - `incenteev/composer-parameter-handler` (Symfony 2/3) — uses `getIO()`
    for interactive prompts + `getPackage()->getExtra()`. → superseded by
    Symfony Flex.
  - `Sensio…DistributionBundle\Composer\ScriptHandler` (Symfony Standard
    Edition) — `getIO()->askConfirmation`, `getConfig()`, `getExtra()`. →
    superseded by Flex.
  - `drupal-composer/drupal-project` ScriptHandler — `getIO()`,
    `getComposer()`. → superseded by `drupal/core-composer-scaffold`.
- **Interactivity is undesirable mid-`sync` anyway** — a script prompting
  during install is exactly what we don't want.
- It keeps bougie a deterministic native tool instead of an embedded
  Composer host.

**What this means concretely:** the supported surface is **non-internal
scripts** — i.e. the process-spawning entry forms (`@php`, `@composer`,
`@putenv`, `@<alias>`, plain shell), which are inherently
internals-free. PHP-callback entries (`Class::method`) are **not
executed**; they are warn-and-skipped, with **one exception**: a small
fixed registry of callbacks bougie reproduces natively
(`Illuminate\Foundation\ComposerScripts::postAutoloadDump` → native
`clearCompiled`; `Composer\Config::disableProcessTimeout` → no-op). That
registry is a curated allowlist, **not** a general callback runner.

Projects that genuinely need a callback's behavior under bougie should
express it as a shell entry (`@php -r "…"` or a real command) — the same
move the rest of the ecosystem made when it went plugin-first.

## MVP vs deferred

**MVP:** command events (`pre/post-install-cmd`, `pre/post-update-cmd`,
`pre/post-autoload-dump`); `Shell` + `@php` + `@putenv` + `@alias` entries;
callbacks = warn/skip + native handlers; opt-in via `bougie.toml` + flags;
unsandboxed; fail-on-nonzero; bin-dir PATH + `COMPOSER_DEV_MODE`;
reconciliation with native Laravel discovery.

**Deferred:** per-package events; `@composer` beyond install/dump-autoload;
sandbox profiles; `bougie composer run-script`; Windows shell-entry polish.

**Non-goals:** executing PHP-callback entries (`Class::method`) or any
script that relies on Composer internals — see the section above. No PHP
shim, no embedded Composer.

## Testing

- `bougie-scripts` unit tests: entry classification (each form), alias
  recursion + cycle detection, `@putenv` scoping, callback registry hit
  vs warn, non-zero-abort.
- Integration (`crates/bougie/tests/`): a project with `[scripts] run =
  true` and a `post-install-cmd` that writes a sentinel file → assert it
  ran; a `--no-scripts` run → assert it didn't; a failing script → assert
  non-zero exit + abort; a Laravel project scripts-on → `packages.php`
  produced by the real `artisan package:discover` (mock artisan) + native
  clearCompiled, no double-write.
- Determinism: scripts-off path is byte-identical to today (regression).

## Open questions

- `bougie.toml [scripts] run = true` vs a per-event allowlist — start
  all-or-nothing.
- Should enabling scripts also imply running them inside `bougie run` /
  `bougie make` task contexts, or only the install lifecycle? (Start:
  install lifecycle only.)
- `@composer dump-autoload` mid-`post-autoload-dump` would recurse into
  bougie's dump — guard against re-entrancy.
