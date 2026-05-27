# Composer Parity Matrix

Defines what "Composer compatibility" means for bougie, at what fidelity
level each behavior is held, and how each claim is tested.

**Reference version:** Composer 2.8.12 (the version fixture scripts pin).

## Parity levels

| Level | Symbol | Meaning | Test shape |
|-------|--------|---------|------------|
| **Byte-equivalent** | `B` | Output is byte-for-byte identical to Composer 2.8.12 | Diff bougie output against Composer output; any difference is a bug |
| **Semantically equivalent** | `S` | Same logical result (packages, versions, structure); formatting may differ | Parse both outputs, compare structured representation |
| **Deliberately different** | `D` | Bougie intentionally deviates; deviation is documented | Assert bougie's behavior; document why it differs |
| **Not yet implemented** | `N` | Composer supports it, bougie doesn't yet; planned | Test that bougie fails gracefully (clear error, not crash) |
| **Out of scope** | `–` | Bougie will never support this | Test that bougie rejects cleanly with actionable message |

---

## 1. Version & constraint parsing

| Behavior | Level | Test strategy | Notes |
|----------|-------|---------------|-------|
| Version normalization (`1.0.0` → `1.0.0.0`, `v2.1` → `2.1.0.0`, `1.0-RC1` → `1.0.0.0-RC1`) | `B` | Layer 1: `conformance.json` ported from `composer/semver` test suite | TSV dataset in `bougie-semver/tests/data/` |
| Constraint parsing (`^1.0`, `~2.1`, `>=1.0 <2.0`, `1.0\|2.0`, `!=1.5`, `1.x`, `*`) | `B` | Layer 1: conformance suite covers all operators | |
| Stability suffixes (`@dev`, `@beta`, `@RC`, `@alpha`, `@stable` in constraints) | `S` | Layer 2: resolver unit tests with per-package stability overrides | |
| Inline aliases (`dev-main as 1.0.0`) | `N` | Carried through lockfile; not re-interpreted by resolver yet | Phase C |
| Branch-alias (`extra.branch-alias`) | `S` | Resolver reads aliases from metadata; test via fixture | |
| `dev-*` branch version ordering | `S` | Dev versions have no natural semver order; alias promotion tested | |

## 2. Dependency resolution

| Behavior | Level | Test strategy | Notes |
|----------|-------|---------------|-------|
| Transitive dependency resolution | `S` | Layer 2: 60+ inline pubgrub fixtures | Same package set as Composer for same inputs |
| `require-dev` inclusion / `--no-dev` exclusion | `S` | Layer 2 + Layer 4 cross-check | |
| `replace` — exact version | `S` | Layer 2: dedicated replace tests | |
| `replace` — range / wildcard (`*`) | `S` | Layer 2: wildcard synthesis tests | |
| `provide` — virtual packages | `S` | Layer 2: provide tests | |
| `conflict` handling | `S` | Layer 2: conflict detection tests; Layer 4: magento corpus validates cross-cutting conflicts | Post-solve validation loop excludes violated versions |
| `minimum-stability` floor filtering | `S` | Layer 2: stability floor tests with dev/alpha/beta/RC/stable | |
| Per-package stability override (`"foo/bar": "^1.0@dev"`) | `S` | Layer 2: per-package flag tests | |
| `prefer-stable` — stable over pre-release when both match | `S` | Layer 2: two-pass selection tests | |
| `prefer-lowest` | `N` | Exposed in lockfile model but not acted on | Phase C |
| Platform requirements (`php`, `ext-*`, `lib-*`) — filtering | `D` | Skipped from solver; checked at install time against resolved PHP + extensions | Solver doesn't use platform info; install preflight enforces |
| `composer-plugin-api` / `composer-runtime-api` | `D` | Filtered out; never constrains resolution | bougie has no plugin API to version |
| Resolution order determinism | `S` | Layer 4: same inputs → same lockfile package set | pubgrub may pick differently when multiple valid solutions exist; assert equivalence only on the package set, not internal solver trace |

### Cross-check corpus (Layer 4)

The resolver cross-check runs `composer update` and `bougie composer update`
on the same `composer.json` + Packagist snapshot, then asserts:

- Same set of resolved package names.
- Same resolved versions for each package.
- No package present in one output but absent in the other.

**Corpus projects** (from RESOLVER_TEST_PLAN):

| Project | Why included |
|---------|-------------|
| `bougie` (this repo) | Eats own dogfood; moderate graph |
| `laravel/laravel` | Wide transitive graph, many replace/provide |
| `symfony/symfony` | Monorepo replace pattern, branch aliases |
| `wordpress/wordpress` | Legacy PSR-0 + path quirks |
| `phpunit/phpunit` | Deep dev-dependency graph |
| `phpstan/phpstan` | Stability flags + dev branches |
| `doctrine/orm` | Virtual packages (DBAL interfaces) |
| `league/flysystem` | Multiple adapter provides |
| `monolog/monolog` | Small focused graph; sanity check |
| `nesbot/carbon` | Inline aliases, Laravel integration |
| `magento/community-edition` | Largest real-world graph (~1000 transitive); stress test |

Each project contributes a frozen Packagist snapshot
(`packagist-index.json.zst`) so tests are offline and deterministic.

## 3. Lockfile (`composer.lock`)

| Behavior | Level | Test strategy | Notes |
|----------|-------|---------------|-------|
| `content-hash` computation | `B` | Unit test: compute hash from fixture `composer.json`, assert matches Composer's | Uses `bougie-php-json` for PHP-compatible `json_encode` |
| Top-level key set (`_readme`, `content-hash`, `packages`, `packages-dev`, `aliases`, `minimum-stability`, `stability-flags`, `prefer-stable`, `prefer-lowest`, `platform`, `platform-dev`, `platform-overrides`, `plugin-api-version`) | `S` | Layer 3: parse Composer-generated lockfile, round-trip, diff | |
| `packages` / `packages-dev` ordering | `S` | Sorted by name (Composer's convention); assert sorted | Composer sorts alphabetically; bougie does the same |
| Per-package field set in lock entries | `S` | Layer 3: assert all Composer fields preserved on round-trip | |
| `dist.shasum` — empty string accepted | `S` | Regression test: GitHub zipballs have `""` shasum | Fixed in #161 |
| Lockfile write (from resolver) | `S` | `bougie composer update` writes `composer.lock` atomically; `--dry-run` previews | |
| Empty `require`/`require-dev` as `[]` vs `{}` | `S` | Accept both on read; emit `{}` on write | Matches Composer's leniency |

### Content-hash test protocol

Content-hash is the one lockfile field where byte-equivalence is
**load-bearing** — `composer install` checks it. Test:

1. Take N real-world `composer.json` files (corpus above).
2. Compute `content-hash` via bougie.
3. Compute `content-hash` via `composer.phar` (call `Composer\Package\Locker::getContentHash`).
4. Assert byte-equal MD5 hex strings.

## 4. Install behavior (`composer install`)

| Behavior | Level | Test strategy | Notes |
|----------|-------|---------------|-------|
| Dist (zip) download + extraction | `S` | Integration test: wiremock serves zips, assert vendor/ tree | |
| Dist cache keying (`shasum` or `reference`) | `S` | Unit test: cache hit/miss scenarios | |
| Parallel download | `D` | bougie parallelizes; Composer is sequential by default | Faster; same result |
| Vendor directory layout (`vendor/<vendor>/<package>/`) | `S` | Integration test: assert directory tree matches Composer | |
| Wrapping directory stripping (zip root dir) | `S` | Fixture zips with/without wrapping dirs | |
| Source (git clone) install | `N` | Error: "source-only packages not yet supported" | Phase D |
| Tar dist install | `N` | Error: "tar dists not yet supported" | Phase A deferred |
| `config.preferred-install` | `N` | Ignored; always uses dist | Phase C |
| `config.vendor-dir` | `N` | Hardcoded to `vendor/` | |
| `config.bin-dir` | `N` | Hardcoded to `vendor/bin/` | |
| Bin proxy scripts in `vendor/bin/` | `S` | PHP proxy + shell proxy matching Composer 2.8.12's `BinaryInstaller` | `.bat` proxies deferred |
| Path repository symlinks | `N` | Recognized but not materialized | Phase D |
| Plugin zip extraction | `–` | Skipped with warning; plugin hooks never run | |
| `post-install-cmd` / `post-update-cmd` scripts | `–` | Warning emitted; never executed | User runs via `bougie run -- composer run-script` |
| Content-hash verification on install | `S` | Fail-fast on mismatch; assert error message | |
| Missing lockfile → error | `S` | Assert actionable error message | |

## 5. Autoloader output (`dump-autoload`)

| Behavior | Level | Test strategy | Notes |
|----------|-------|---------------|-------|
| `vendor/autoload.php` | `B` | Byte-equivalence harness: 15 fixture suites | |
| `vendor/composer/autoload_psr4.php` | `B` | Byte-equivalence harness | |
| `vendor/composer/autoload_namespaces.php` (PSR-0) | `B` | Byte-equivalence harness | |
| `vendor/composer/autoload_classmap.php` | `B` | Byte-equivalence harness | |
| `vendor/composer/autoload_files.php` | `B` | Byte-equivalence harness | |
| `vendor/composer/autoload_real.php` | `B` | Byte-equivalence harness | |
| `vendor/composer/autoload_static.php` | `B` | Byte-equivalence harness | |
| `vendor/composer/ClassLoader.php` | `B` | Vendored copy; assert identical | |
| `vendor/composer/InstalledVersions.php` | `B` | Vendored copy; assert identical | |
| `vendor/composer/installed.json` | `S` | Structural equivalence (key set, values) | Field ordering may differ |
| `vendor/composer/installed.php` | `B` | Byte-equivalence harness | |
| `vendor/composer/platform_check.php` | `N` | Not yet emitted | AUTOLOADER_PLAN.md |
| `--optimize` / `-o` flag | `B` | Fixture: `psr4-optimize` | |
| `--classmap-authoritative` / `-a` flag | `B` | Fixture: `classmap-authoritative` | |
| `--no-dev` flag | `B` | Fixture coverage across suites | |
| `--apcu-autoloader` flag | `B` | Fixture: `apcu-autoloader` | |
| `--apcu-autoloader-prefix` | `B` | Fixture: `apcu-autoloader` with explicit prefix | |
| `config.autoloader-suffix` | `B` | Fixture: `autoloader-suffix` | |
| `exclude-from-classmap` globs | `B` | Fixture: `classmap-exclude` | |
| `target-dir` (legacy PSR-0) | `N` | Rarely used; deferred | |
| Files ordering (dependency-aware) | `B` | Fixture: `files-deps-order` | |

### Autoloader fixture protocol

Each of the 15 fixture suites works as follows:

1. `input/` contains `composer.json`, `composer.lock`, and source files.
2. `expected/` contains the reference `vendor/composer/` output generated
   by `scripts/generate-autoload-fixtures.sh` using Composer 2.8.12.
3. The test runs bougie's autoloader on `input/` and diffs against
   `expected/` byte-for-byte.
4. Any fixture that drifts after a Composer version bump must be
   regenerated and the diff reviewed.

## 6. JSON encoding

| Behavior | Level | Test strategy | Notes |
|----------|-------|---------------|-------|
| `json_encode($data, 0)` (content-hash mode): escape `/`, escape non-ASCII as `\uXXXX` | `B` | `bougie-php-json` unit tests | Required for content-hash equivalence |
| `json_encode($data, JSON_PRETTY_PRINT \| JSON_UNESCAPED_SLASHES \| JSON_UNESCAPED_UNICODE)` (file mode): 4-space indent, raw `/`, raw UTF-8 except U+2028/U+2029 | `B` | `bougie-php-json` unit tests | Required for `installed.json` / lockfile equivalence |
| Empty array `[]` vs empty object `{}` | `S` | Accept both on read; emit the PHP-canonical form on write | PHP's `json_encode` emits `[]` for empty arrays and `{}` for empty objects; Composer relies on this |

## 7. Authentication

| Behavior | Level | Test strategy | Notes |
|----------|-------|---------------|-------|
| Project `auth.json` (next to `composer.json`) | `S` | Integration test: assert credentials sent on HTTP request | |
| Global `auth.json` (`$COMPOSER_HOME/auth.json`, XDG fallbacks) | `S` | Integration test: mock global path, assert credentials sent | |
| `COMPOSER_AUTH` env var (JSON) | `S` | Integration test: set env, assert credentials sent | |
| `config.http-basic` in `composer.json` | `S` | Integration test: embedded auth, assert header | |
| `config.bearer` in `composer.json` | `S` | Integration test: embedded bearer, assert header | |
| Precedence: `COMPOSER_AUTH` > project `auth.json` > `composer.json` config > global `auth.json` | `S` | Integration test: multiple sources, assert highest wins | Recently fixed in last commit |
| GitHub OAuth token (`github-oauth`) | `S` | Parsed from auth.json / COMPOSER_AUTH; sends `Authorization: token <tok>` | Matches Composer's `x-oauth-basic` sentinel behavior |
| GitLab token (`gitlab-token`) | `S` | Parsed from auth.json / COMPOSER_AUTH; sends `PRIVATE-TOKEN: <tok>` | String and `{username, token}` object formats |
| GitLab OAuth (`gitlab-oauth`) | `S` | Parsed from auth.json / COMPOSER_AUTH; sends `Authorization: Bearer <tok>` | |
| Bitbucket OAuth (`bitbucket-oauth`) | `N` | Not yet implemented | |
| Interactive auth prompts | `–` | bougie never prompts for credentials at resolve time | Use `bougie auth add` or auth.json |
| Credential redaction in error output | `S` | Unit test: assert `<redacted>` in debug strings | |

## 8. Repository types

| Behavior | Level | Test strategy | Notes |
|----------|-------|---------------|-------|
| Implicit `packagist.org` | `S` | Default behavior; tested in every resolver test | |
| `{"packagist.org": false}` disabling | `S` | Layer 2: test with disabled Packagist | |
| `type: "composer"` (Satis, Artifactory, Private Packagist) | `S` | Layer 2 + Layer 4: custom repo URL, assert metadata fetched | |
| Packagist v2 protocol (`/p2/<name>.json`) | `S` | Layer 2: wiremock serves v2 responses | |
| Packagist v1 protocol (`provider-includes` + per-package) | `S` | Layer 2: v1 fallback tests | |
| `type: "vcs"` (git, GitHub, GitLab, Bitbucket) | `N` | Error with message | Phase D |
| `type: "path"` (local directory) | `N` | Error with message | Phase D |
| `type: "package"` (inline definition) | `N` | Error with message | |
| `type: "artifact"` (directory of zips) | `N` | Error with message | |
| Repository priority (first match wins) | `S` | Layer 2: test with overlapping repos | Composer uses first-listed repo for a given package |
| ETag / conditional GET caching | `S` | Unit test: sidecar file written, 304 honored | |
| `canonical: false` on repos | `N` | Not yet implemented | Composer 2.x feature |

## 9. Config (`composer.json` → `config`)

| Behavior | Level | Test strategy | Notes |
|----------|-------|---------------|-------|
| `config.platform` (override PHP/ext versions) | `S` | Participates in content-hash; resolver will use in Phase C | |
| `config.platform-check` | `N` | Controls `platform_check.php` emission; not yet | |
| `config.sort-packages` | `S` | Honored on `require`/`require-dev` mutation | |
| `config.autoloader-suffix` | `B` | Passed through to autoloader; tested in fixtures | |
| `config.optimize-autoloader` | `N` | Parsed but not acted on automatically | |
| `config.classmap-authoritative` | `N` | Parsed but not acted on automatically | |
| `config.apcu-autoloader` | `N` | Parsed but not acted on automatically | |
| `config.preferred-install` (`dist` / `source` / per-package map) | `N` | Not honored; always dist | Phase C |
| `config.vendor-dir` | `N` | Hardcoded to `vendor/` | |
| `config.bin-dir` | `N` | Hardcoded to `vendor/bin/` | |
| `config.cache-dir` | `–` | bougie uses its own cache layout | |
| `config.data-dir` | `–` | bougie uses its own data layout | |
| `config.secure-http` | `N` | Not enforced | |
| `config.github-protocols` | `N` | Not relevant without VCS repos | Phase D |
| `config.github-domains` / `config.gitlab-domains` | `N` | Not yet implemented | |
| `config.use-parent-dir` | `–` | bougie does not walk up to find `composer.json` | Deliberate difference |
| `config.allow-plugins` | `–` | bougie never runs plugins | |
| `config.process-timeout` | `–` | bougie never spawns PHP processes during install | |

## 10. CLI behavior

| Behavior | Level | Test strategy | Notes |
|----------|-------|---------------|-------|
| `composer install` | `S` | Integration tests (Layer 4) | |
| `composer update` | `S` | Dry-run only; lockfile writing is Phase C | |
| `composer dump-autoload` | `B` | Byte-equivalence harness | |
| `composer require` | `N` | Not yet implemented | → `bougie add` |
| `composer remove` | `N` | Not yet implemented | → `bougie remove` |
| `composer show` / `composer info` | `N` | Not yet implemented | → `bougie outdated` partially |
| `composer outdated` | `N` | Not yet implemented | TODO item |
| `composer validate` | `S` | Name, license (SPDX), constraints, autoload, lock freshness, publish checks | `--strict`, `--no-check-lock`, `--no-check-publish`, `--no-check-all` |
| `composer search` | `–` | Out of scope | Use Packagist web |
| `composer create-project` | `–` | Out of scope | → `bougie init --starter` |
| `composer global` | `–` | Out of scope | → `bougie tool` |
| `composer run-script` | `–` | Out of scope | → `bougie run` |
| `composer exec` | `–` | Out of scope | → `bougie run` |
| `composer archive` | `–` | Out of scope | |
| `composer diagnose` | `–` | Out of scope | |
| `composer audit` | `N` | Not yet implemented | TODO item (vulnerability scanning) |
| `composer fund` | `–` | Out of scope | |
| `composer licenses` | `–` | Out of scope | |
| `composer suggests` | `–` | Out of scope | |
| `composer check-platform-reqs` | `N` | Not yet implemented | Related to `platform_check.php` |
| `--working-dir` / `-d` flag | `S` | Tested in integration tests | |
| `--no-dev` flag | `S` | Tested across install/update/dump-autoload | |
| Exit codes (0 = success, 1 = generic error, 2 = dependency resolution failure) | `S` | Assert exit codes in integration tests | |
| `--dry-run` on update | `S` | Only mode currently; tested | |
| `--ignore-platform-reqs` | `S` | Accepted on install + update; bougie skips platform checks by default | |
| `--ignore-platform-req=<name>` | `S` | Accepted on install + update; bougie skips platform checks by default | |

## 11. Error behavior

| Behavior | Level | Test strategy | Notes |
|----------|-------|---------------|-------|
| Version conflict derivation tree | `D` | pubgrub's `DefaultStringReporter`; format differs from Composer | Assert conflict is reported with involved packages; don't assert exact wording |
| Content-hash mismatch error | `S` | Assert error contains old + new hash and actionable message | |
| Missing `composer.json` error | `S` | Assert actionable error message | |
| Missing `composer.lock` on install | `S` | Assert error suggests running update | |
| Network failure errors (metadata fetch) | `S` | Assert URL + context in error message | |
| Auth failure (401/403) | `S` | Assert host + actionable message | |
| Unsupported features (source, tar, plugins) | `D` | Assert clear error/warning, not crash | bougie fails gracefully where Composer would succeed |

## 12. Deliberately out of scope (never)

These Composer features will never be implemented in bougie. Each must
produce an actionable error or warning when encountered.

| Feature | Why excluded | Behavior on encounter |
|---------|-------------|----------------------|
| Plugin execution | bougie has no PHP runtime at install time; security boundary | Warning listing detected plugins |
| Script execution (`pre-install-cmd`, `post-install-cmd`, etc.) | Same as plugins; reimplemented natively where needed | Warning listing declared scripts |
| `composer global` | Replaced by `bougie tool` | N/A (different command surface) |
| Interactive credential prompts | bougie uses file/env-based auth | Error with instructions to configure auth.json |
| `config.allow-plugins` | No plugins to allow | Ignored |
| `config.process-timeout` | No PHP processes spawned | Ignored |

---

## Testing layers (summary)

| Layer | What it tests | Where | Depends on |
|-------|---------------|-------|------------|
| **L1** — Semver conformance | Version/constraint parsing matches `composer/semver` | `bougie-semver/tests/conformance.rs` | `conformance.json` from upstream test suite |
| **L2** — Solver unit fixtures | Resolution logic (replace, provide, stability, conflicts) | `bougie-composer-resolver/src/update/tests.rs` | Embedded fixtures, wiremock |
| **L3** — Lockfile fidelity | Round-trip, content-hash, field preservation | `bougie-composer/src/lockfile.rs` tests, `bougie-composer-resolver/src/verify/` | Embedded fixtures |
| **L4** — Cross-check harness | Same inputs → same outputs vs real `composer.phar` | `bougie/tests/composer_cross_check.rs` | Frozen Packagist snapshots; 9 corpus projects passing |
| **L5** — Derivation snapshots | Error message regression | Inline in resolver error paths (planned) | |
| **LA** — Autoloader byte-equiv | `dump-autoload` output matches Composer 2.8.12 | `bougie-autoloader/tests/byte_equivalence.rs` | 15 fixture suites, `generate-autoload-fixtures.sh` |

### Layer 4 cross-check protocol

```
for each corpus project:
  1. Load frozen composer.json + packagist-index.json.zst
  2. Run Composer: `composer update --no-install` → composer.lock
  3. Run bougie: `bougie composer update --dry-run` → resolved set
  4. Assert:
     a. Same package names (set equality)
     b. Same versions per package
     c. Content-hash matches (if bougie writes lockfile)
  5. If (a) or (b) fails:
     - Diff is a bug unless the project has known ambiguity
       (multiple valid solutions). Document ambiguities per-project.
```

### Fixture regeneration

When bumping the reference Composer version (currently 2.8.12):

1. Run `scripts/generate-autoload-fixtures.sh` with the new phar.
2. Diff all `expected/` directories. Review every change.
3. Run `scripts/port-composer-semver-tests.php` against the new
   `composer/semver` release. Diff `conformance.json`.
4. Re-run Layer 4 corpus. Any new diff is a Composer behavior change
   that bougie must decide to follow or document as deliberate divergence.
5. Update this document's reference version.
