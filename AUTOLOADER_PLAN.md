# bougie autoloader — implementation plan

Working plan for shipping `bougie composer dump-autoload`: a native
Rust port of Composer's `AutoloadGenerator` + `ClassMapGenerator`.
First concrete deliverable from the broader `RESOLVER_PLAN.md`
roadmap. Chosen first because it is fully self-contained (no
resolver, no fetcher, no async, no semver), has an obvious test
methodology (byte-diff against `composer dump-autoload`), and ships
as a usable standalone subcommand.

Performance is a top-line goal, not an afterthought. The classmap
generator is the single slowest step of `composer install` on
medium-to-large projects — a Laravel install spends multiple seconds
in it. Composer is PHP-bound, single-threaded, and uses PCRE.
Replacing that with Rust + `rayon` + SIMD-capable byte scanners
should give a substantial speedup, and that speedup is what makes
this subcommand interesting on its own (otherwise users would just
keep running `composer dump-autoload`).

## Mental model

The output of `composer dump-autoload` is a fixed set of files under
`vendor/autoload.php` and `vendor/composer/*`. We re-create them
byte-for-byte (or as close as the relevant Composer release allows;
see "Compatibility target"). No PHP execution involved.

```
vendor/
  autoload.php                          # the entry point — tiny
  composer/
    autoload_real.php                   # ClassLoader singleton initializer
    autoload_namespaces.php             # PSR-0
    autoload_psr4.php                   # PSR-4
    autoload_classmap.php               # classmap
    autoload_files.php                  # files for eager-load (in dependency order)
    autoload_static.php                 # static form of all of the above
    ClassLoader.php                     # vendored verbatim from Composer
    InstalledVersions.php               # vendored verbatim from Composer
    installed.json                      # written by install, not dump-autoload
    installed.php                       # ditto
    LICENSE                             # vendored verbatim from Composer
    platform_check.php                  # if config.platform-check is on
```

## Compatibility target

We pin to a specific upstream Composer release for byte-equivalence.
Pin: **Composer 2.8.x** (current stable as of plan-writing). The
pinned version is checked into `crates/bougie-autoloader/vendored/`
alongside its SHA and the `composer/class-map-generator` version it
shipped with. A `xtask refresh-composer-files` updates both
atomically.

We only promise byte equality for **the autoload files**. Files like
`installed.json` are written by install, not by `dump-autoload`, and
fall under `RESOLVER_PLAN.md`'s installer scope.

## Hard scope boundaries

Inherited from `RESOLVER_PLAN.md` — bougie does not run plugins or
scripts. Concretely for this subcommand:

- Composer's `dump-autoload` dispatches `PRE_AUTOLOAD_DUMP` and
  `POST_AUTOLOAD_DUMP` events to allow plugins (and user scripts via
  `composer.json` `scripts.pre-autoload-dump` / `post-autoload-dump`)
  to mutate the output. Bougie does not. If those scripts are
  declared, bougie warns (lists which ones); if a `composer-plugin`
  in the lock declares an autoload-generator capability, bougie
  hard-errors and points the user at plain Composer.

## Surface (CLI)

```
bougie composer dump-autoload [-o | --optimize]
                              [-a | --classmap-authoritative]
                              [--apcu]
                              [--no-dev]
                              [--strict-psr]
                              [--ignore-platform-reqs]
                              [--dry-run]                # don't write, only verify
```

Aliases: `bougie composer du` (matches Composer's habit), `bougie
composer dumpautoload` (Composer accepts both, so we do too).

Default behavior matches Composer's default: PSR-4/PSR-0/files map,
explicit classmap entries only (no scan of PSR-* dirs). `-o` adds
the PSR-* directory scan. `-a` skips the PSR-* fallback entirely
(classmap-only resolution). `--apcu` adds APCu caching wrapper
code to `autoload_real.php`. `--strict-psr` errors on PSR
violations during the scan (otherwise warnings, like Composer).

## New crate

```
crates/bougie-autoloader/
  Cargo.toml
  vendored/
    composer-<version>/
      ClassLoader.php
      InstalledVersions.php
      LICENSE
      platform_check.php.tmpl
    SHA256SUMS
    VERSION
  src/
    lib.rs                 # public API: generate(GenerateOpts) -> Generated
    opts.rs                # GenerateOpts, strict flags
    manifest.rs            # walk composer.lock + composer.json into Packages
    scan/
      mod.rs               # parallel scan orchestrator
      cleaner.rs           # PhpFileCleaner port
      finder.rs            # regex extractor (post-match lookbehind check)
      walker.rs            # directory enumeration
      exclude.rs           # exclude-from-classmap matcher
    emit/
      mod.rs
      real.rs              # autoload_real.php
      psr4.rs              # autoload_psr4.php + autoload_namespaces.php
      classmap.rs          # autoload_classmap.php
      files.rs             # autoload_files.php
      static_.rs           # autoload_static.php
      entry.rs             # vendor/autoload.php
      platform_check.rs    # platform_check.php from the .tmpl
      php_value.rs         # tiny PHP literal serializer
    vendored.rs            # copies + sha-verifies the pinned files
```

Dependencies:

- `rayon` — parallel CPU work for the classmap scan.
- `regex` — already in workspace; we use the non-lookbehind subset and
  do post-match byte checks for the negative-lookbehind bits.
- `memchr` — SIMD byte search. `regex` already pulls it in.
- `walkdir` or `ignore::WalkBuilder` — directory enumeration. The
  `ignore` crate is faster (parallel walk) but pulls more deps;
  start with `walkdir` and switch if benchmarks justify.
- `bstr` — `[u8]`-first string handling for the cleaner / finder.
  PHP files are nominally UTF-8 but not guaranteed; working over
  `&[u8]` avoids a validation pass.
- `sha2` — already in workspace; we use it to verify the vendored
  files at build time and at runtime first-write.

## Performance goals

Concrete targets, measured on an M-series Mac:

| Project              | Composer 2.8 baseline | Bougie target |
| ---                  | ---                   | ---           |
| bougie repo itself   | ~0.3s                 | ≤0.05s        |
| symfony/demo         | ~0.8s                 | ≤0.15s        |
| laravel/laravel + breeze | ~1.5s             | ≤0.25s        |
| magento community     | ~6s                  | ≤1s           |

Roughly: aim for **5–10x faster on cold runs, 50x on cached re-runs**.
The big gap on Magento comes from its enormous classmap (~250k
classes) — that's where parallelism + SIMD-backed scanning have the
most room. Targets are aspirational; refine after first benchmark.

We commit to maintaining a `cargo bench` harness from Phase 1, so
regressions show up in CI.

## Phasing

Three internal phases. Each is shippable, each is testable
independently against fixtures.

### Phase 1 — PSR-4 + PSR-0 + files (no classmap)

**Ship:** `bougie composer dump-autoload` that produces correct
`autoload_psr4.php`, `autoload_namespaces.php`, `autoload_files.php`,
`autoload_real.php`, `vendor/autoload.php`, vendored ClassLoader /
InstalledVersions / LICENSE. Default Composer mode (no `-o`/`-a`)
that doesn't require classmap scanning.

**Why first:** No file scanning involved. Pure data transformation
from `composer.lock` + `composer.json` `autoload` blocks to a set of
PHP-source files. Forces the manifest reader, the lockfile-to-
package walker, and the PHP-literal emitter into shape, but the
expensive part (classmap) is deferred. Fixes the test loop early
(byte-diff against Composer output on a fixture project).

**Performance focus:** trivial here — the work is bounded by the
size of the lockfile. Expect ~1ms even for huge projects.

**Acceptance:** byte-identical output to Composer 2.8 on 5+ real-
world projects using only `psr-4`/`psr-0`/`files` autoload schemes.

### Phase 2 — Classmap scanning with parallelism + SIMD

**Ship:** `--optimize`, `--classmap-authoritative`, and the implicit
classmap pass for explicit `autoload.classmap` directory entries.
This is the bulk of the work.

**Why second:** Foundations from Phase 1 (manifest, emitter, vendored
files) are tested. Adds the parallel scanner and the PHP-source
mini-lexer. The performance budget gets exercised seriously here.

**Performance focus:** see "Classmap pipeline" below.

**Acceptance:** byte-identical output on the same fixtures as Phase 1
plus 3+ additional fixtures specifically chosen for classmap stress
(magento, drupal, laminas — large explicit classmaps or large
PSR-scan surfaces).

### Phase 3 — `autoload_static.php`

**Ship:** static-loader file emission. Default-on in modern Composer
(2.x).

**Why last:** Largest emit surface, most format-fragility risk. Once
Phase 2's classmap is correct, this is "serialize the same data in a
different layout."

**Acceptance:** byte-identical static file on all fixtures. Hardest
target because Composer's static-loader formatting has version-
dependent quirks (PHP version checks, array packing).

## Classmap pipeline

The performance-critical path. Five stages, each parallelizable
where it helps.

### 1. Enumerate candidate files

For each `autoload.psr-4`, `psr-0`, `classmap` entry across all
packages + the root, expand to a list of `.php` / `.inc` / `.hh`
files. Composer's order is dependency-topological so that downstream
packages can override.

Implementation: `walkdir` recursive walk per directory entry. File
list per package is collected serially per package, but packages
can be walked in parallel via `rayon::scope`. Output: a `Vec<(path,
PackageId)>`.

`exclude-from-classmap` regex match happens here, before any I/O on
the file itself.

### 2. Read + clean per file (parallel)

`rayon::par_iter` over the file list. For each file:

- `std::fs::read(&path) -> Vec<u8>`. We use `&[u8]` throughout, no
  UTF-8 validation. PHP source is normally ASCII-safe in the bits
  we care about (identifiers, keywords, delimiters).
- Run the regex prefilter
  (`\b(?:class|interface|trait|enum)\b`) against the **raw**
  source. If no match, skip the file — no class to find. Composer
  does this against the post-`php_strip_whitespace` source; we get
  the same effect against raw source because the keywords don't
  show up in valid comments or strings (they could, but the false-
  positive rate is acceptable since we run the cleaner before the
  real extraction anyway).
- If the prefilter matched, run `PhpFileCleaner` (port of
  `composer/class-map-generator`'s `PhpFileCleaner.php`).
- Run the extraction regex on the cleaned source.
- Resolve the lookbehind manually: for each match, check the byte
  before the match's start. Skip if it is `\` / `$` / `:` / `>`.
  This replaces Composer's `(?<![\\\\$:>])` in a way the `regex`
  crate supports.

Output per file: `Vec<(class_name, file_path)>` plus a per-file
error or warning list.

**SIMD opportunities here:**

- `PhpFileCleaner` uses `strcspn` in PHP — "advance until a byte in
  set X." Rust equivalent: `memchr::memchr3` and friends, which are
  SSE4.2 / AVX2 / NEON SIMD-accelerated. Most of the cleaner's hot
  loop is "skip to the next delimiter," which maps directly. Build
  the bytemask once per type-config and reuse.
- The regex prefilter benefits from the `regex` crate's internal
  Teddy multi-literal SIMD search (free, no extra code).
- The extraction regex benefits from the same.
- For very large files (>1MB, rare in vendor/), `memmap2` to avoid
  an extra copy. Bench-gated, not default.

### 3. Aggregate

Reduce the per-file vectors into a single `BTreeMap<ClassName,
FilePath>` for deterministic output ordering. `rayon::reduce` or
`fold` + merge.

Ambiguity handling: if two files declare the same class name,
Composer emits a warning and keeps the first one (per package
dependency order). We mirror this exactly. The map is built per
package (parallel), then merged in package dependency order
(sequential merge — fast since each per-package map is small).

### 4. PSR-* fallback in optimized mode

In `--optimize` mode, classes found via PSR-4/PSR-0 directory scans
also enter the classmap. The scan is the same pipeline as step 2,
just over the PSR-* directory roots instead of `classmap`
directories.

`--classmap-authoritative` makes the classmap the *only* lookup —
PSR-* runtime fallback is disabled. Affects emit, not scan.

### 5. Emit

PHP-source emission for the classmap is a single function: take the
`BTreeMap`, emit `return array(...)` with each entry on its own
line. Composer's output is sorted and uses single-quoted strings
with backslash-escaped namespace separators. Trivial to match.

Parallelism here is fine-grained — the four emit files (`psr4`,
`namespaces`, `classmap`, `files`) can be written in parallel via
`rayon::scope`, each going through a `BufWriter`.

## PhpFileCleaner port

The single most performance-critical piece of code in the project.
Direct port of `composer/class-map-generator/src/PhpFileCleaner.php`
(reference: `RESOLVER_PLAN.md`-adjacent context, ~248 PHP lines).

Behavior:

- Walk a `&[u8]` source.
- Until `<?` is seen, copy nothing (we're in HTML mode).
- After `<?`, copy non-string/non-comment content through; replace
  string and heredoc contents with the literal `null`.
- Track `?>` to exit PHP mode.
- Bail out of cleanup once `maxMatches` (number of class-shaped
  keywords seen) has been consumed (PHP-side optimization for
  files with exactly one class — Composer special-cases this at
  `PhpFileCleaner.php:120`).

Rust implementation notes:

- Single allocation: the output `Vec<u8>` sized at `input.len()` up
  front (output ≤ input).
- `memchr` for every "skip to delimiter" loop. Composer's PHP uses
  `strcspn` and we have a faster equivalent.
- No regex inside the cleaner — Composer uses `Preg::isMatch` only
  for the heredoc-start pattern (`<<<EOT`), which is fixed shape
  and parses easily by hand.
- Zero-copy where possible: pass through long runs of "boring"
  bytes by extending output by a slice rather than byte-by-byte.

Benchmark this in isolation against a curated PHP-file corpus
(`vendor/symfony/symfony/src/Symfony/Component/HttpFoundation/*.php`
+ a synthetic worst-case file with many strings + heredocs).
Target: ≥500MB/s single-threaded throughput on M-series.

## PHP-literal emitter

Composer emits PHP source like:

```php
<?php
return array(
    'Composer\\Autoload\\ClassLoader' => $vendorDir . '/composer/ClassLoader.php',
    'Composer\\InstalledVersions' => $vendorDir . '/composer/InstalledVersions.php',
    ...
);
```

We need a small emitter that produces:

- `array(...)` long-array form (Composer uses long form, not `[]`,
  for compat with PHP 5.x — even though 5.x is dead, the format is
  fossilized).
- Single-quoted strings with `\\` and `\'` escapes.
- The `$vendorDir . '/...'` and `$baseDir . '/...'` prefix tokens —
  these are PHP variables, not strings, so they have to be emitted
  as PHP literals (i.e., unquoted in the output).

This is ~50 lines of Rust. The only nuance is the variable-prefix
handling, which is mechanical: known prefix → emit
`$vendorDir . 'rest'`; otherwise emit a fully-quoted string.

## Caching across runs

Composer doesn't cache the classmap output between invocations.
Bougie can. A `vendor/composer/autoload_classmap.bougie-cache.json`
records, for each scanned file, `(mtime, size, sha256-of-input,
extracted-classes)`. On subsequent runs:

- If `mtime + size` match, trust the cache (fast path, no hashing).
- Otherwise rehash; if hash matches, trust.
- Otherwise rescan.

This is opt-in via `[autoloader] cache = true` in `bougie.toml`
because it changes on-disk state. Off by default in Phase 2;
default-on in Phase 3 if it proves correct on the fixture suite.

This is the difference between the 5–10x speedup (cold) and the
50x speedup (warm) in the perf targets. Worth doing, but not on
the critical path for shipping Phase 2.

## File ordering and determinism

The four output files are sensitive to ordering:

- `autoload_files.php` order is **dependency topological** — when a
  package's `files` autoload entry depends on a function defined by
  another package, the dependency must be loaded first. Composer
  determines this from `composer.lock`'s package order, which is a
  topological sort.
- `autoload_psr4.php` and `autoload_namespaces.php` order is **by
  prefix length descending**, then alphabetical, so longer prefixes
  match first.
- `autoload_classmap.php` is **alphabetical by class name**.

These orderings are deterministic given a fixed lockfile, so byte-
equivalence with Composer is achievable.

The classmap parallel scan is non-deterministic in *processing*
order, but deterministic in *output* because we sort before emit.
Same for per-package merges — each per-package map is built
deterministically (single-threaded reduce over its files); merges
happen in package dependency order.

## Testing

Fixture-driven, two layers:

1. **Per-package autoload-block fixtures.** Hand-crafted minimal
   `composer.json` + `vendor/<pkg>/` trees exercising each
   `autoload` schema variant (psr-4, psr-0 with target-dir, files,
   classmap, exclude-from-classmap, mixed). 15–20 fixtures.
2. **Full-project fixtures.** Real-world `composer install`
   outputs vendored as test data: bougie itself, symfony/demo,
   laravel/laravel + breeze, drupal core, magento community,
   phpstan/phpstan, phpunit/phpunit, league/flysystem,
   monolog/monolog, doctrine/orm.

For each fixture: run Composer's `dump-autoload` (using a pinned
`composer.phar` from the build), then run `bougie composer
dump-autoload`, then `diff -r vendor/composer/autoload_*.php
fixture/expected/autoload_*.php`.

CI gate: byte-identical for every fixture, every commit. A single
mismatch fails the build.

Performance regression tests: `cargo bench` over the same fixtures,
recording wall-clock time. CI compares against the previous commit's
numbers; >10% regression fails the build.

## Risks / open questions

1. **Static-loader format fragility (Phase 3).** Composer's
   static-loader emitter has version-dependent quirks (PHP version
   checks emitted as runtime branches, particular array packing
   tricks for performance). Byte equivalence here may force the
   most work. Mitigation: pin to one Composer minor at a time,
   refresh deliberately.
2. **Modern PHP syntax in classmap scan.** PHP 8 attributes
   (`#[Attribute]`), enums (already handled in upstream), readonly
   class properties, new-in-initializers — none change the cleaner
   logic, but worth a syntax-coverage fixture pass.
3. **Files with mixed PHP and HTML.** Realistic in legacy code,
   rare in modern `vendor/`. The cleaner handles `?>` exit, but
   weird edge cases (PHP open tag without close, `<?=` short echo)
   need explicit handling.
4. **Memory pressure on big projects.** Magento's classmap is
   ~250k entries × ~50 bytes per `(name, path)` = ~12.5MB. Plus
   intermediate per-file `Vec<u8>` allocations during scan. Should
   be fine; flag if benchmarks show GC-like pressure from
   allocator churn (consider `bumpalo` for per-scan arena).
5. **Caching correctness.** A cached entry can lie if a file is
   replaced with one of identical mtime+size but different content
   (`make` shenanigans). The sha256 hash check at the second tier
   defends against this. Worth a stress test on the cache layer
   specifically.
6. **Composer 2.x vs 3.x.** Composer 3 will likely ship before
   this lands. The vendored-files pin lets us track one major at
   a time, but we should design for "two pins coexist," not
   "monkey-patch the format." Defer until 3 actually ships.

## Sequencing

- Phase 1: 2–3 weeks. Most of it is the manifest reader + PHP-
  literal emitter; emit is mechanical.
- Phase 2: 3–4 weeks. PhpFileCleaner port + extractor + parallel
  scan + benchmarks. Largest phase.
- Phase 3: 1–2 weeks. Static-loader emission; risky but bounded.

Total: ~2 months of focused work for a usable
`bougie composer dump-autoload` that's 5–10x faster than the
upstream.
