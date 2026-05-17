# bougie composer-resolver — implementation plan

Working plan for shipping a native, pubgrub-based Composer dependency
resolver and installer inside bougie. Replaces the round-trip through
`composer install` / `composer update` for projects bougie manages,
with the goal of cutting cold-start resolution and `vendor/` install
time the way `uv` did for `pip`.

The solver itself is the smallest piece. Most of the work is in
faithfully encoding Composer's domain (constraint syntax, stability,
`replace`/`provide`, platform packages, lockfile fidelity) and in the
async metadata fetcher that makes the resolver actually fast.

Phases are bottom-up; stop at any phase and the preceding work still
has standalone value.

## Mental model

Bougie's two pre-existing scopes are **toolchain** (PHP, Composer-the-
binary, extensions, dev server) and **lockfile bookkeeping** for
extensions (hand-editing composer.json/composer.lock without re-
solving). This plan adds a third: **project dependency resolution and
install**. Concretely, a `bougie composer` surface that can:

1. Install a project's `vendor/` from an existing `composer.lock`
   without invoking the Composer binary.
2. Verify that a `composer.lock` is a valid solution for a
   `composer.json`, with no network access required.
3. Re-resolve the dependency graph from scratch (`composer update`
   semantics), producing a `composer.lock` byte-compatible with what
   Composer would write.

The relationship to the upstream `composer/composer` binary is
deliberate and unchanged for the surfaces bougie already owns:

| Surface                          | Resolver used         | Network        | Composer binary involved? |
| ---                              | ---                   | ---            | ---                       |
| `bougie ext add/remove`          | none (lock hand-edit) | no             | no                        |
| `bougie composer install`        | none (apply lock)     | dist downloads | no (this plan, Phase A)   |
| `bougie composer install --lock-verify` | pubgrub (offline) | no            | no (Phase B)              |
| `bougie composer update`         | pubgrub               | metadata + dist | no (Phase C)             |
| Repo types bougie doesn't implement yet (VCS, artifact) | per-package | dist downloads | **dist falls back to `composer.phar`** |

The fallback path above is **only** for repository types bougie's
installer hasn't implemented yet (VCS sources in Phase C, etc.). It
is **not** a general escape hatch — bougie does not run Composer
plugins or scripts under any circumstances. See "Hard scope
boundaries" below.

## What we are **not** building

Out of scope, intentionally:

- A general-purpose pubgrub crate. We use upstream `pubgrub` from
  crates.io (the same one uv uses). Our crate is the Composer-shaped
  `DependencyProvider` and the surrounding plumbing.
- Source installs from VCS for the MVP (Phase D, deferred).
- Composer plugins, of any kind. See "Hard scope boundaries" below.
- Composer scripts: `pre-install-cmd`, `post-install-cmd`,
  `pre-update-cmd`, `post-update-cmd`, `pre-autoload-dump`,
  `post-autoload-dump`, per-package install scripts. See "Hard
  scope boundaries" below.
- A Packagist-compatible repository server. We *read* Packagist v2
  metadata; we don't *serve* it.
- Cross-repo replacement indexes. We use what Packagist exposes
  (`/p2/<name>.json` declares `replace`/`provide` on each version),
  loaded lazily — see "replace / provide" below.
- A `bougie-composer-server` analogue to `uv tool server`. If a
  long-running resolver daemon turns out to be useful (it might, for
  IDE integration), it lands separately, not in this plan.

## Hard scope boundaries

Two things bougie will **never** do, no matter what phase, no matter
how popular the feature is. Both follow from the same principle:
bougie owns project resolution and installation natively, and the
projects it supports are the ones that can resolve and install
deterministically with no PHP execution.

### Composer plugins are unsupported

Composer plugins (`type: composer-plugin`) are PHP-side extensions
that hook into the resolver, installer, autoload generator, and event
bus. Examples: custom installers (`oomphinc/composer-installers-extender`,
`drupal/core-composer-scaffold`), `wikimedia/composer-merge-plugin`,
hirak/prestissimo (legacy), composer-runtime-api consumers.

We don't run them. There is no plugin compatibility shim, no
"detect and fall back to composer.phar" path, no allowlist of
known-safe plugins. Booting PHP to execute plugin code would
re-create the very thing the native resolver exists to replace.

**Detection + behavior:** at resolve time, if any package in the
solution has `type: composer-plugin`, `bougie composer install` /
`update` emits a hard error pointing the user at plain Composer:

> This project requires Composer plugins
> (`<vendor/name>`, `<vendor/name>`...), which bougie does not
> support. Use plain Composer for this project.

A `--allow-plugins=ignore` flag is available for users who know the
plugin doesn't affect resolution or installation in a way they care
about (e.g., a plugin that only adds CLI commands). The flag does
**not** run the plugin; it only suppresses the hard error so install
can proceed. Misusing it produces broken installs; that is the
user's problem.

### Composer scripts are unsupported

The `scripts` block in `composer.json` runs arbitrary PHP / shell at
specific lifecycle points: `pre-install-cmd`, `post-install-cmd`,
`pre-update-cmd`, `post-update-cmd`, `pre-autoload-dump`,
`post-autoload-dump`, per-package `pre-package-install` etc., plus
user-defined custom scripts. Common uses: `php artisan
package:discover` after install, clearing app caches, copying assets,
regenerating IDE helper files.

We don't run them. Same reasoning as plugins.

**Detection + behavior:** on install/update, bougie reads
`composer.json`'s `scripts` block. If any lifecycle script is
declared (the user-defined custom-script entries like
`"scripts": { "test": "phpunit" }` invoked via
`composer run test` are fine — bougie's `bougie run` already
handles those), emit a warning listing the unrun scripts:

> composer.json declares lifecycle scripts that bougie does not
> run: post-install-cmd, post-update-cmd. If your project depends
> on these, run them manually (`php artisan package:discover` etc.)
> or use plain Composer.

`--ignore-scripts` exists as a no-op flag for parity with Composer's
CLI; it suppresses the warning. The warning is the default because
silently skipping a script the user is counting on is worse than
noisy success.

### Why this boundary, stated plainly

A project that needs plugins or scripts to install correctly is a
project bougie does not support. That is not a temporary state. The
target users are projects where `composer install` is a pure
resolve + download + extract + autoload-dump operation, which covers
the large majority of PHP libraries and a substantial fraction of
applications. Frameworks that lean hard on `post-install-cmd`
(Laravel, Symfony with recipes, some Drupal setups) and projects
using merge-plugin or custom installers are outside the envelope.
Those users continue to run plain Composer; bougie's `bougie run`
already exposes the Composer binary for that case.

## Surface (CLI)

Adds to the existing `bougie composer` namespace. The crate currently
owns binary management (`bougie composer install <version>`); the new
subcommands operate on the project's `composer.json` instead. The
disambiguation rule is: a positional argument that parses as a
Composer-binary version request → binary management; absence of one
(or `--project`) → project deps.

```
bougie composer install              [--no-dev] [--lock-verify] [--frozen]
                                     [--ignore-platform-reqs]
                                     [--ignore-platform-req=<name>]...
bougie composer update [<pkg>...]    [--no-dev] [--with-all-deps]
                                     [--prefer-lowest] [--prefer-stable]
                                     [--minimum-stability=<stab>]
                                     [--dry-run]
bougie composer require <pkg>[:<constraint>]... [--dev] [--no-update]
bougie composer remove <pkg>...      [--dev] [--no-update]
bougie composer lock                 # alias for `update --no-install`
bougie composer why <pkg>            # derivation walk; see "Errors"
bougie composer why-not <pkg>[:<constraint>]
```

`install` without a lockfile errors and suggests `update`, matching
Composer's behavior. `install --frozen` refuses to install if the lock
is out-of-sync with `composer.json` (CI usage). `install --lock-verify`
runs Phase B's offline verifier and exits — no `vendor/` touched.

Long-term goal is one-to-one parity for the verbs above. Flags that
exist in Composer but are unsupported get a clear error pointing at
the fallback (`use \`bougie run -- composer <verb> <flag>\``).

## Pre-existing bougie code we lean on

The repo just finished an unbundling pass; all paths below are
post-unbundle (commit `4427cc3`).

- `crates/bougie-composer/src/lockfile.rs` — `composer.json` /
  `composer.lock` IO, `content_hash`, the PHP-compatible top-level
  ksort, `require_add` / `require_remove`, lock platform set/unset.
  Already does the round-trip we need; the resolver writes through it.
- `crates/bougie-composer/src/php_json.rs` — already parses the bits
  of `composer.json` we read.
- `crates/bougie-version/src/version.rs`, `request.rs` — bougie's own
  version + constraint types, used today for PHP version requests. The
  Composer constraint dialect is similar enough that the *grammar*
  (winnow combinators) can be shared, but the *semantics* differ (see
  "Version model"). Likely outcome: a new `composer-semver` submodule
  inside the new crate, structurally mirroring `bougie-version` but
  with Composer's rules.
- `crates/bougie-fetch/src/lib.rs` — TLS + signature + retry. The
  resolver's metadata client builds on it. Today it's blocking
  reqwest; the resolver needs async (see "Metadata fetcher").
- `crates/bougie-fs/src/store.rs` + `lock.rs` — content-addressed
  cache + flock. Packagist metadata caching reuses both shapes.
- `crates/bougie-config/src/composer.rs` — reads the
  `[composer]` section of `bougie.toml` (mirror URLs, auth tokens).
  The resolver consumes the same config.
- `crates/bougie-errors/src/lib.rs` — `BougieError::Resolution` is
  already the right variant; add `kind = "composer-dep"` and a new
  derivation-tree payload for `why` output.
- `crates/bougie-cli/src/lib.rs` — clap surface; new
  `ComposerCommand::Update`, `::Require`, `::Remove`, `::Lock`,
  `::Why`, `::WhyNot` variants land here.

Conspicuously **not** reused: `crates/bougie-resolver`. That crate
resolves PHP / extension manifests from the bougie index. Same word,
different problem. We don't extend it; we add a sibling.

## New crate layout

Two new workspace members. `bougie-semver` is the shared substrate
(see "Shared substrate: `bougie-semver`" below for the reasoning);
`bougie-composer-resolver` is the solver + installer.

```
crates/bougie-semver/
  Cargo.toml
  src/
    lib.rs                # re-exports
    version.rs            # the Version type (see "Version model")
    constraint.rs         # Constraint + Range (intersect, union, contains)
    parser.rs             # winnow grammar
    stability.rs          # Stability enum + min-stability rules

crates/bougie-composer-resolver/
  Cargo.toml              # depends on bougie-semver
  src/
    lib.rs                # public API: resolve(), install(), verify()
    pubgrub/
      mod.rs
      package.rs          # PubGrubPackage enum
      provider.rs         # DependencyProvider impl
      priority.rs         # version-preference policy
    metadata/
      mod.rs              # Packagist v2 fetcher
      cache.rs            # on-disk metadata cache
      index.rs            # in-memory parsed metadata
    install/
      mod.rs              # apply a lock: download, extract, autoload
      autoload.rs         # PSR-4 / PSR-0 / classmap / files
      downloader.rs       # parallel dist downloads
    fallback.rs           # detect "we can't handle this" + dispatch
    lock.rs               # read/write composer.lock (delegates to bougie-composer for content_hash)
    error.rs              # derivation-aware error types
```

Naming rationale: `bougie-composer-resolver` sits next to
`bougie-composer` (binary management) and clearly distinct from
`bougie-resolver` (PHP/ext index resolution). Three crates with
"resolver" or "composer" in the name is awkward but each owns a
genuinely different problem; renaming any of them is a separate fight
we don't pick here.

Dependencies (new to `bougie-composer-resolver`):

- `pubgrub` = upstream solver. Pin to the same major as uv to stay
  on a known-good version line.
- `tokio` + `reqwest` async — `bougie-fetch` is blocking today.
  Easiest path: a new `bougie-fetch-async` sibling that shares config
  + cert pinning + retry policy, or a feature flag in `bougie-fetch`.
  Decide during Phase C kickoff — Phase A only needs the existing
  blocking client.
- `futures` + `futures-concurrency` for the prefetcher.
- `rustc-hash` for the interning maps (matches uv's choice; tiny but
  measurable in solver hot loop).

## Shared substrate: `bougie-semver`

The composer-flavored semver types live in their own crate, not buried
inside `bougie-composer-resolver`, so that the existing extension
resolver can use them too. The motivating observation:

`bougie-resolver` (the PHP / ext index resolver shipped at
`crates/bougie-resolver/`) is a *picker*, not a solver — its
docstring describes filter-then-pick-highest, no transitive
dependency graph. Pubgrub adds nothing there today; the algorithm is
already correct. But the *constraint algebra* it needs is the same
one the new resolver needs:

- `bougie-resolver::version_matches_spec` / `version_matches_partial`
  is "does this version satisfy this constraint" — i.e.,
  `Range::contains(Version)`.
- `bougie-resolver::intersect_php` (`crates/bougie-resolver/src/lib.rs:145`)
  intersects `composer.json` `require.php` with the bougie pin —
  i.e., `Range::intersect(Range)`.

Both today are hand-rolled against bougie's own version request
grammar. Both should be replaced with calls into `bougie-semver`
once it exists, so the two resolvers agree on what `^8.3 || ^8.4`
means down to the bit.

What `bougie-semver` owns:

- `Version` (4-segment + stability)
- `Range` with `contains`, `intersect`, `union`, `complement`
- `Constraint` (parsed surface form: `^1.0`, `~2.5`, `>=1 <2`,
  `1.2.3 || 1.3.x`, stability suffixes)
- Winnow parser
- `Stability` enum + ordering
- Implements `pubgrub::VersionSet` for `Range` so it slots into the
  solver crate without an adapter

What stays in `bougie-resolver`:

- Yanked filtering
- Flavor filtering (PHP minor + ext)
- `pick_highest`
- Index-row representation (`Selected`, `ResolveOptions`)

Migration is a single small PR (separate from the resolver work):
introduce `bougie-semver`, port `bougie-resolver`'s
hand-rolled version-matching to it, delete the duplicated logic.
Lands before Phase C ideally, so the constraint parser has two
consumers exercising it before the solver gates on it. If the
migration uncovers a behavioral difference between bougie's existing
PHP-version-request grammar and Composer's dialect, that's a
finding worth surfacing — they *should* agree where they overlap
(`^8.3`, `>=8.2`), and any divergence is either a Composer-side
extension we encode at parse time or a bougie-side bug we fix.

### When the ext resolver would actually want pubgrub

Today, the extension index has no transitive constraints — extensions
don't depend on each other or on libraries in the bougie model. If
that changes — concretely, if the index ever encodes:

- `ext-pdo_mysql` requires `ext-pdo` loaded, **or**
- `ext-imagick` requires `lib-magick >= 7`, **or**
- Two extensions can conflict (e.g., `ext-mysqli` vs. `ext-mysqlnd`)

— then the ext resolver becomes a real solver problem and switching
it to `bougie-composer-resolver`'s pubgrub machinery becomes
worthwhile. Until that happens, the picker is the right tool, and
this plan does not touch it beyond the `bougie-semver` migration.

## On-disk layout

Reuses the existing bougie cache. Per `bougie-paths`:

```
$BOUGIE_CACHE/
  composer-metadata/                   # Packagist v2 cache
    p2/
      <vendor>/
        <name>.json                    # mtime-validated; ETag stored alongside
        <name>~dev.json
    last-modified/<name>               # for conditional GETs
  composer-dist/                       # downloaded zip/tar dists
    <sha1>.<ext>                       # keyed by composer's documented dist hash
```

The dist cache is content-addressed by Composer's published dist
`shasum`, so multiple projects sharing a dep share bytes. This is what
Composer's own `~/.composer/cache/files/` does, just under bougie's
strict-XDG layout.

No new files in the project directory beyond what Composer already
writes (`composer.lock`, `vendor/`). We don't introduce a separate
bougie-flavored lockfile — round-tripping the canonical
`composer.lock` is a hard requirement (see "Lockfile fidelity").

## Phasing

Each phase ends in a shippable bougie release. Phase boundaries are
where the public CLI surface or on-disk format changes; internal
refactors slide between them as needed.

### Phase A — install-from-lock (no solver)

**Ship:** `bougie composer install` that reads an existing
`composer.lock`, verifies `content-hash` against `composer.json`,
parallel-downloads dists, extracts to `vendor/`, and generates
autoloader files. No scripts run, no plugins activated (see "Hard
scope boundaries").

**Why first:** Most users run `install` ≫ `update`. The performance
win — from sequential blocking reqwest to a tokio-backed parallel
downloader — is the visible value of this whole initiative, and it
needs zero solver code. Phase A is also where the lockfile reader, the
content-hash check, the autoloader generator, and the dist cache all
land. Every subsequent phase depends on them.

**Out of scope for this phase:**
- Any resolver. `install` on a `composer.json` without a lockfile
  errors with `run \`bougie composer update\` first`.
- VCS sources. `dev-*` versions referencing git refs fall back to
  `composer.phar` for the dist download only.
- Plugins and scripts: not run, ever. Hard-error or warn as described
  in "Hard scope boundaries."

**Acceptance:** for the bougie repo itself + ~10 representative
real-world Composer projects (Laravel skeleton, Symfony demo,
WordPress install, Magento 2 community, Drupal core, phpunit/phpunit,
phpstan/phpstan, doctrine/orm, league/flysystem, monolog/monolog), the
resulting `vendor/` is byte-identical to what `composer install`
produces, modulo timestamps. Compared by `find vendor -type f | sort |
xargs sha256sum`.

### Phase B — lock-verify (read-only solver)

**Ship:** `bougie composer install --lock-verify` and an internal
guard inside `install` that runs the verifier when `--frozen` is
implied by CI heuristics. Pubgrub-driven: given `composer.json` and
`composer.lock`, the verifier constructs a `DependencyProvider` whose
only candidates are the versions named in the lock, runs pubgrub, and
reports either "valid" or a derivation tree explaining the
inconsistency.

**Why second:**
- It exercises the Version model, constraint parser, and platform-
  package encoding against real lockfiles without needing network
  access or replace/provide handling (the lock already names the
  chosen replacer).
- It's where the derivation-aware error reporting first earns its
  keep, on simpler inputs than Phase C.
- CI users get a fast "is this lock still valid?" check that doesn't
  hit Packagist.

**Out of scope:**
- Anything network-touching. The lock names every package + version
  we'll consider.

**Acceptance:** for the same fixtures as Phase A, verify passes. For
constructed broken lockfiles (mismatched content-hash, version
violating a require, missing transitive dep), verify fails with a
derivation tree that names the conflicting clauses.

### Phase C — full update

**Ship:** `bougie composer update`, `require`, `remove`, `lock`,
`why`, `why-not`. This is the resolver in its full form: lazy
Packagist v2 metadata loading, parallel prefetching, replace/provide
handling, stability flags, repository priorities, lockfile write at
byte-compatible fidelity.

**Why last:**
- Largest scope, most edge cases, slowest to validate against the
  Composer reference.
- Builds on every piece Phases A + B forced into shape.

**Out of scope (still):**
- VCS / source / path / artifact repositories. These fall back to
  `composer.phar` for the affected packages' dist downloads, while
  pubgrub handles resolution for everything else. Mixed-mode is a
  complication we accept rather than block on a clean solution;
  Phase D revisits.
- Plugins (any kind) and scripts (lifecycle hooks). See "Hard scope
  boundaries" — these projects are unsupported, not fallback-routed.

**Acceptance:** for each fixture project, `bougie composer update` on
a wiped `composer.lock` produces a lock that:
1. Has identical package selections (`name@version` set) to what
   `composer update` produces on the same input,
2. Validates with `composer install --dry-run` on the reference
   Composer,
3. Is byte-identical to the canonical Composer output after
   running both through `jq --sort-keys` (we don't promise byte
   equality of the actual file because Composer's writer has minor
   formatting quirks we may not perfectly mirror; we *do* promise
   semantic equality).

### Phase D — VCS / source installs

Deferred. Likely shape: a separate downloader backend for git sources,
keyed by ref, with the solver treating `dev-*` versions as a separate
package namespace (see "Version model"). Calls out to `git` rather
than reimplementing it. May or may not land in bougie depending on how
often the fallback to `composer.phar` proves acceptable.

## Version model

The Composer version dialect has three flavors that don't share a
single total order:

1. **Semver-shaped** versions: `1.2.3`, `1.2.3-RC1`, `1.2.3-beta.2`,
   etc. Standard ordering after Composer's normalization (`-RC1` →
   `-RC1`, `-beta` → `-beta`, etc., with documented precedence).
2. **dev versions**: `dev-master`, `dev-feature/foo`,
   `1.x-dev`. These point at git refs and have **no natural order
   against each other or against semver versions**. Composer cheats
   by letting users attach a *branch alias* (`"branch-alias":
   {"dev-master": "1.0.x-dev"}`), which promotes the dev version onto
   the semver line.
3. **`as`-aliased versions**: `dev-master as 1.0.x-dev`, used inline
   in `require`. Same trick as branch-alias but at the requirer side.

Pubgrub requires `Version: Ord`. We split namespaces rather than
inventing a total order:

```rust
pub enum PubGrubPackage {
    Root,
    Php,
    Extension(String),     // ext-*
    Library(String),       // lib-*
    Package(PackageName),
    DevPackage(PackageName),
    Replaces(PackageName), // see "replace / provide"
}
```

`Package(name)` carries semver-shaped versions only; `DevPackage(name)`
carries dev versions only. A requirement for `name` that allows both
(e.g., `^1.0 || dev-master`) becomes two clauses targeting the two
packages, with the requirer also pulling in a single shared "pick one"
constraint via a small virtual `Proxy` (the same trick uv uses for
extras — see analysis notes saved before this plan was written).

Version representation:

```rust
pub struct Version {
    major: u32,
    minor: u32,
    patch: u32,
    extra: u32,                    // composer normalizes to 4-segment
    stability: Stability,          // Stable < RC < Beta < Alpha < Dev (reverse for compare)
    stability_num: u16,            // -RC1 vs -RC2
}
```

Stability lives *inside* `Version` so the natural `Ord` does the right
thing. Range expressions (`^1.0`, `~2.5`, `>=1 <2`, etc.) compile down
to a `pubgrub::Range<Version>`; we implement the conversion in
`bougie-semver`'s `constraint.rs`. The grammar lives in
`bougie-semver`'s `parser.rs` and uses the same winnow combinator
style as `bougie-version`.

**Equivalence to Composer:** we will, at minimum, pass Composer's
own `composer/semver` test suite by porting the JSON cases. CI gates
on this.

## `replace` / `provide`

The single most awkward piece of the encoding. Composer lets package A
declare `"replace": {"B": "^1.0"}`, meaning that installing A
satisfies a requirement for B (B is not separately installed). A
related declaration is `"provide": {"psr/log-implementation": "1.0"}`,
which provides a virtual capability without forbidding a separate
install.

Pubgrub has no native concept of "this package satisfies that
requirement." We encode it via virtual packages plus lazy discovery:

```
PubGrubPackage::Replaces(name)
```

is a virtual package whose "version" is a `(replacer_name,
replacer_version)` tuple. A requirement for B becomes a disjunction
across two pubgrub packages:

- `Package(B) @ <range>`
- `Replaces(B) @ <any pair whose replace range intersects the original range>`

The hard part is that `Replaces(B)`'s candidate set is *only known
after fetching the replacers' metadata*. We can't enumerate replacers
without scanning. Three paths:

1. **Eager scan** — at startup, fetch every package's metadata. Not
   tractable; the universe is too large.
2. **Replacer index** — Packagist exposes per-name metadata that
   *declares* replaces, but there is no documented inverse index. We
   could build one client-side by scanning incrementally, but it
   means cold solves are slow.
3. **Lazy, with backtracking** — the resolver starts assuming no
   replacers exist. When a `Package(B)` requirement fails (no
   satisfying version), the metadata fetcher checks whether any
   currently-loaded package declares `replace.B`. Newly-discovered
   replacers are added to `Replaces(B)`'s candidate set and pubgrub
   backtracks. Cost: extra solver work on failed sub-resolutions.

We commit to (3). It mirrors what Composer's PoolBuilder effectively
does: extra passes when initial assumptions don't pan out.
`replace`/`provide` declarations on already-loaded packages are
accumulated into an in-memory `ReplaceIndex` keyed by replaced-name;
every package fetch updates it. When a `Package(B)` clause is about
to fail, we consult the index, add any new `Replaces(B)` candidates,
and let pubgrub continue.

`provide` differs from `replace` semantically: `provide` doesn't
prevent a separate install of the provided package, so it doesn't
satisfy a hard requirement *unless* the requirer accepts virtual
satisfaction. In practice we treat the two identically at the
resolver layer (both make `Replaces(B)` candidate) and let the
selection policy in `priority.rs` prefer real over virtual when both
are available, matching Composer's behavior.

## Platform packages

Direct port of uv's `Python`/`System` pattern.

- `Php` — version = the detected (or bougie-pinned) PHP version.
- `Extension(name)` — version = the loaded version of the extension,
  or `0.0.0.0` if loaded but unversioned. Absence = no candidate, so
  any `ext-foo` require fails with a derivation pointing at the
  missing extension.
- `Library(name)` — version = the version reported by the runtime
  (libssl, libcurl, etc.). Detected via the same mechanism Composer's
  `PlatformRepository` uses (parse `phpinfo()`-equivalent output from
  the bougie-managed PHP).

`--ignore-platform-reqs[=<name>]` replaces the affected packages with
unconstrained `Range::full()` candidates so the resolver always
succeeds on them. Useful for cross-platform lockfiles produced on a
different host than they'll install on (which Composer also
supports).

## Stability handling

Encoded into the `Range`, not as a side filter, so the solver never
even considers out-of-stability versions. `minimum-stability: beta` +
`prefer-stable: true` means the *range* on each requirement excludes
versions weaker than beta unless the requirer explicitly opted in
with `@<stability>`. The policy in `priority.rs` (analogous to uv's
`CandidateSelector`) then prefers stable within the remaining set.

This differs from a naive design where stability is filtered after
selection — getting it wrong leads to spurious backtracking when the
solver picks a too-unstable version and only later notices.

## Dependency provider

Pubgrub's `DependencyProvider` trait has two methods we care about:

```rust
fn choose_version(&mut self, package: &P, range: &Range<V>) -> Option<V>;
fn get_dependencies(&mut self, package: &P, version: &V) -> Result<Dependencies<P, V>>;
```

Our `provider.rs` implements both against a `MetadataIndex` (loaded
Packagist data) plus a `Priorities` map. `choose_version` consults:

1. Locked versions (if a previous lockfile exists and we're in
   `--prefer-locked` mode, default for `install --lock-verify` and
   `update <pkg>` partial updates),
2. URL-pinned versions (`composer.json` repositories of type `path`
   or `vcs` with explicit ref),
3. The highest version in the range under stability rules,
4. `--prefer-lowest` flips this to lowest.

`get_dependencies` consults the in-memory metadata, fetching it
lazily if not yet loaded (this is where the async prefetcher
underneath us is critical).

## Metadata fetcher

Packagist v2 protocol: two files per package, both gzipped JSON:

```
/p2/<vendor>/<name>.json        # stable versions
/p2/<vendor>/<name>~dev.json    # dev versions
```

Both are large (hundreds of KB to MB for popular packages). We:

1. Use HTTP conditional GET (`If-Modified-Since` against the cached
   `last-modified/<name>` sentinel).
2. Cache parsed `MetadataIndex` entries in memory for the duration
   of the solve.
3. Prefetch in a background tokio task: while the solver works on one
   package's `get_dependencies`, the prefetcher pulls the top-N
   not-yet-loaded packages mentioned in already-known dependency
   lists, ranked by `Priorities`. Direct port of the uv strategy in
   `crates/uv-resolver/src/resolver/batch_prefetch.rs`.

The prefetcher is the largest single source of wall-clock improvement
over Composer. Composer's resolver is algorithmically fine; it just
fetches sequentially.

## Lockfile fidelity

We round-trip `composer.lock` through `bougie-composer`'s existing
lockfile primitives. Specifically:

- `content_hash` is already implemented; we call it.
- The top-level `ksort` is already implemented; we call it.
- Nested key order: Composer preserves source order inside `packages`,
  `packages-dev`, etc. We do the same — order is determined by the
  topological sort the solver produces, which matches Composer's
  algorithm for the same inputs.
- `aliases`, `minimum-stability`, `stability-flags`, `prefer-stable`,
  `prefer-lowest`, `platform`, `platform-dev`, `platform-overrides`,
  `plugin-api-version` — all preserved.
- `packages[].dist` and `.source` — `dist` is what we install from
  (zip URL + sha1); `source` we copy through from metadata unchanged.
  Phase D would actually consume `source`.

We test byte equivalence (mod minor formatting) against fixtures in
Phase C acceptance.

## Errors

Pubgrub's derivation tree is the headline feature for error reporting.
Composer's existing error messages are pretty good for simple cases
and pretty bad for deep conflicts. Our `error.rs` walks the derivation
and renders one of:

- **Single-line**: `package X cannot be installed: requires Y ^2.0 but
  composer.json forbids Y ^2`.
- **Two-step**: `package X 1.0 requires Y ^2.0, but Y 2.0 requires Z
  ^1, and composer.json requires Z ^2`.
- **Tree**, for deeper conflicts: indented, with each step labeled.

`bougie composer why <pkg>` and `why-not <pkg>[:<constraint>]` are
direct queries over the same derivation infrastructure. `why` walks
*successful* derivations ("X is installed because A requires B
requires X"); `why-not` walks the failure tree.

## Per-package fallback to `composer.phar` for unimplemented repo types

Narrow fallback path, scoped only to *features bougie hasn't
implemented yet*. Triggers:

- A `composer.json` `repositories` entry of a type whose downloader
  bougie doesn't ship yet (`vcs`, `git`, `artifact` in Phase C;
  `path` we handle natively). The resolver decides what to install;
  the per-package downloader shells out to `composer.phar` for the
  actual fetch + extract of those specific packages.
- An explicit `--via-composer` flag for debugging or A/B comparison
  against the reference implementation.
- During the first N releases, an opt-in `[composer] cross-check =
  true` setting that runs `composer install --dry-run` after every
  bougie resolve and aborts on divergence. Slow but safe; expected
  to be turned off once the cross-check harness in CI has bedded in.

Explicitly **not** triggers for fallback (covered by hard errors
instead, per "Hard scope boundaries"):

- Presence of plugins.
- Presence of scripts.

The narrow fallback lives in `fallback.rs` and shells out to
`bougie-composer`'s installed Composer binary for the affected
packages only.

## Testing strategy

Four test layers:

1. **`composer/semver` port** — JSON test cases from the upstream
   composer/semver repo, loaded by a build.rs equivalent into our
   constraint-parser tests. Gates parser changes.
2. **Solver fixtures** — small hand-written `composer.json` +
   expected-`composer.lock` pairs covering: simple semver,
   stability flags, `replace`, `provide`, conflict, platform reqs,
   `--prefer-lowest`, partial updates. ~30 fixtures by end of
   Phase C.
3. **Cross-check harness** — `tests/composer_cross_check.rs` (in the
   top-level `bougie` crate's integration tests, so it can exec
   `bougie` end-to-end). Runs `bougie composer update` and
   `composer update` on the same input, compares the lockfiles. Fed
   by a curated list of real-world projects.
4. **Snapshot tests for `why` / `why-not`** via `insta` — derivation
   output is sensitive to refactors and benefits from snapshot
   review.

The cross-check harness is gated to a non-default cargo feature
(`composer-cross-check`) because it requires `composer.phar` on PATH.
CI runs it; `cargo test` locally without composer installed skips it
cleanly.

## Risks / open questions

1. **`replace`/`provide` edge cases.** The lazy-discovery approach
   handles the common cases (one fork of a popular package) but may
   miss the long tail where a deep transitive replace influences a
   shallow choice. Phase C acceptance against real-world projects is
   where we find out. If the cross-check failure rate is >5% at the
   end of Phase C, we revisit and consider the eager-index approach.
2. **Project compatibility scope.** Bougie won't run plugins or
   scripts (see "Hard scope boundaries"). Open question is how
   loudly to error. Current plan: hard-error on plugin-using
   projects, warn-and-proceed on script-using projects. We'll see
   how that lands when real users hit it; could tighten scripts to
   hard-error or loosen plugins to warn-and-proceed, depending on
   feedback. Won't change the underlying boundary.
3. **Async vs blocking.** Bougie's HTTP stack is currently blocking
   reqwest. Phase A doesn't need async (a thread pool for parallel
   downloads is enough). Phase C does (the prefetcher's whole value
   is overlapping I/O with solving). Decision deferred to Phase C
   kickoff: either widen `bougie-fetch` with a `tokio` feature, or
   sit the async client purely inside the resolver crate. The latter
   is cleaner; the former amortizes the async stack across future
   bougie features.
4. **Lockfile byte equality.** We promise semantic equality (passes
   `composer install`) but not byte equality. Whether to push for
   byte equality is a Phase C late-stage decision; depends on how
   much it matters to users that `git diff` on `composer.lock` is
   empty after a bougie-run update.
5. **Two crates with "resolver" in the name.** `bougie-resolver`
   (PHP/ext picker) and `bougie-composer-resolver` (Composer dep
   solver) is confusing. Tolerable; `bougie-semver` between them
   makes their relationship legible (one shared substrate, two
   consumers solving different problems). Renaming `bougie-resolver`
   to something like `bougie-index-resolver` is a separate
   discussion.
6. **Re-export through `bougie-composer`.** Open: do callers go
   through `bougie-composer-resolver` directly, or does
   `bougie-composer` re-export the public surface so all
   Composer-shaped APIs live behind one door? Lean toward the
   re-export approach for ergonomics; decide when the public API
   stabilizes in Phase B.

## Sequencing

Approximate work breakdown by phase. Sizing is rough; each "unit" is
~a week of focused work.

- Phase A (install-from-lock): 4–6 units. Most of the work is the
  autoloader generator (PSR-4 / PSR-0 / classmap / files, byte-
  compatible with Composer's `vendor/autoload.php` + `composer/`
  files) and the parallel downloader, not the lockfile reading.
- Phase B (lock-verify): 2–3 units. Version model + constraint
  parser + minimal `DependencyProvider` against fixed candidates.
  Big surface but no async, no fetcher, no replace.
- Phase C (full update): 8–12 units. Metadata fetcher + prefetcher +
  replace/provide + selection policy + lockfile writer + derivation
  reporter + cross-check harness against real projects.
- Phase D (VCS / source): unscoped. Revisit after Phase C lands and
  we know how often the fallback is enough.

Phase A could ship before any pubgrub work is done. Phase B is the
"foundations are sound" milestone. Phase C is the visible
"bougie does what uv does" milestone.
