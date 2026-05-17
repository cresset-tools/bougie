# Test suite for `bougie-composer-resolver` + `bougie-semver`

## Context

`RESOLVER_PLAN.md` sketches a four-layer testing strategy (composer/semver port, hand-written solver fixtures, cross-check harness, derivation snapshots) in ~30 lines. This document expands that sketch into the *full* test suite: where each test lives, what it asserts, what gates it, and how the layers compose into the CI signal that lets us merge resolver changes safely.

The driving design pressures:

- The resolver is a faithfulness project. Almost every bug looks like "bougie's output differs from Composer's." Tests must make divergence loud and locatable, not just visible.
- The four phases (A install-from-lock, B lock-verify, C full update, D VCS) ship independently. The test suite has to come up phase by phase — Phase A can't depend on Phase C scaffolding existing.
- Bougie's convention (per existing crates) is inline `#[cfg(test)]` units with embedded fixtures; integration tests live in `crates/bougie/tests/`. No `insta`, no `tests/data/`, no fixtures dir. New tests should match — no inventing infrastructure that isn't already there.
- Hard scope: bougie does not run Composer plugins or scripts ([[feedback-no-composer-plugins]], [[feedback-no-composer-scripts]]). The cross-check harness MUST select fixtures that don't depend on either; we are not testing equivalence under behavior bougie deliberately doesn't implement.

## Layout

Five test layers, listed in order they come online during Phase A–C.

### Layer 1 — `composer/semver` conformance port  (lands with `bougie-semver`)

**Where:** `crates/bougie-semver/src/constraint.rs` and `src/version.rs`, inline `#[cfg(test)] mod tests` blocks.

**What:** Port the JSON test cases from upstream `composer/composer`'s `tests/Composer/Test/Semver/` into embedded `const` arrays. Two suites:

- *Version comparison cases* — pairs `(a, b, expected_ordering)` covering normalization (`1.0` ↔ `1.0.0.0`), stability ordering (`1.0-RC1 < 1.0`), `dev-*` vs semver, branch aliases.
- *Constraint matching cases* — triples `(constraint_str, version_str, expected_bool)` covering `^`, `~`, `>=`, `||`, `,`, stability suffixes (`@dev`), wildcard (`1.2.*`).

**How they get in:** A one-time vendor step (committed): `scripts/port-composer-semver-tests.py` reads upstream JSON, emits a Rust file with `const CASES: &[(...)] = &[...]`. Re-run when we bump the pinned Composer reference. The script is documentation, not CI infrastructure; the emitted Rust file is the test.

**Gates:** every PR. Parser changes that drop a single case fail CI.

**Anti-goal:** do not write our own version-comparison cases here. This layer is *equivalence to Composer*, period. Bespoke edge cases that Composer doesn't have a case for go in Layer 2.

### Layer 2 — Solver unit fixtures  (lands with Phase B + grows through Phase C)

**Where:** `crates/bougie-composer-resolver/src/pubgrub/provider.rs` and `src/lock.rs`, inline `#[cfg(test)]`. Fixtures embedded as `const FIXTURE_*_JSON: &str` raw-string literals — same shape as `bougie-composer/src/lockfile.rs:428`.

**Scope:** ~30 small `composer.json` + expected-lock pairs by end of Phase C. Categories:

- **Trivial** (3–5): single dep, two-dep, transitive chain. Exercise the happy path.
- **Stability** (4–6): `minimum-stability: dev`, `prefer-stable: true`, per-package `@dev` suffix, RC vs stable preference.
- **Constraint shapes** (4): `^`, `~`, `>=, <`, `||`-union, wildcard.
- **`replace` / `provide`** (4–6): replacer wins, two replacers conflict, `provide` doesn't block separate install, transitive replace discovered late (forces backtrack — see RESOLVER_PLAN.md "replace / provide" path 3).
- **Platform** (3): `require php`, `require ext-*`, `--ignore-platform-reqs` masks unknown ext.
- **Conflict** (4): mutual exclusion via `conflict`, version range with no satisfier, replacer conflicts with separate install of replaced package.
- **Partial update** (2): `update <pkg>` keeps other locks pinned; `update --with-all-deps`.
- **Prefer-lowest** (1): same input, opposite selection.

**Fixture format:** test asserts `resolve(fixture_json) == parse_lock(fixture_lock)` for the *set* of `(name, version)` pairs. Byte-equivalence of the lockfile is Layer 4's job; Layer 2 asserts semantic correctness.

**Determinism:** fixtures use only versions present in an embedded `MetadataIndex` literal — no network, no cache, no async runtime. The `DependencyProvider` for these tests is constructed from a `HashMap<PackageName, Vec<Version>>` plus per-version dependency lists, all inline.

**Gates:** every PR.

### Layer 3 — Lockfile fidelity tests  (lands with Phase A)

**Where:** `crates/bougie-composer-resolver/src/lock.rs`, inline `#[cfg(test)]`.

**What:** byte-equivalence asserts on lockfile *writing*. Three flavors:

1. **Round-trip**: parse a known-good `composer.lock` (embedded const), re-serialize, assert byte-equal.
2. **Content-hash compatibility**: takes a `composer.json` (embedded), computes `content_hash` via `bougie-composer::lockfile::content_hash` (already tested at `bougie-composer/src/lockfile.rs:481`), assert it matches an expected hex string generated by real Composer. This test mostly exists to gate any accidental drift in `bougie-composer`'s implementation as the resolver starts depending on it.
3. **Writer determinism**: same resolver input → same lockfile output, byte-equal, run 100 times. Catches hash-map iteration ordering bugs.

**Gates:** every PR. Layer 3 is small and cheap.

### Layer 4 — Cross-check harness vs. real `composer.phar`  (lands mid-Phase C)

**Where:** `crates/bougie/tests/composer_cross_check.rs` — an integration test in the binary crate, following the pattern of existing `tests/phase9_composer.rs`. Uses the existing `tests/common/mod.rs` `TestEnv` harness.

**What:** for each fixture project, run both:

- `bougie composer update` (via `assert_cmd` against the bougie binary)
- `composer update` (via `assert_cmd` against `composer.phar` on PATH)

Compare:

- **Resolved package set** `{(name, version), ...}` — hard equality.
- **Vendor tree** after `install` — `find vendor -type f | sort | xargs sha256sum`, hard equality modulo timestamps.
- **Lockfile** — semantic equality via `serde_json::Value` comparison after sorting `packages[]` by name. Byte-equality is *not* asserted at this layer (it's a Phase C late-stage decision per RESOLVER_PLAN.md risk #4).

**Fixture corpus** (10 projects, pinned by `composer.lock` commit SHA, fetched into a tempdir by the harness):

| Project | Why it's in the corpus |
| --- | --- |
| bougie itself | dogfood; tiny; PHP-side `composer.json` we control |
| laravel/laravel skeleton | huge transitive graph; common case |
| symfony/demo | many small packages; tight version pins |
| WordPress (composer/installers) | unusual installer-type packages |
| phpunit/phpunit | dev-only deps, stability flags |
| phpstan/phpstan | self-contained, exercises platform reqs |
| doctrine/orm | replace/provide via doctrine/persistence |
| league/flysystem | many adapters, mixed stability |
| monolog/monolog | tiny; PSR-3 provide |
| nesbot/carbon | tight PHP version constraint, branch aliases |

Each fixture has a `composer.json` pinned + a `composer.lock` snapshot committed. The harness:

1. Wipes `vendor/` and `composer.lock`.
2. Runs `bougie composer update`, captures resulting lock.
3. Wipes again.
4. Runs `composer update`, captures resulting lock.
5. Asserts equivalence.

**Skip rule:** before running, scan the project's `composer.json` for plugin or script declarations. If present, **skip the fixture** with a logged reason — we deliberately don't implement those, so divergence isn't a bug.

**Gates:** `--features composer-cross-check`, opt-in. CI runs the feature; `cargo test` locally without `composer.phar` on PATH skips cleanly. Matches the existing `test-fixtures` precedent (`.github/workflows/ci.yml`).

**Network policy:** the harness uses a wiremock-fronted Packagist proxy *only* for bougie's side, populated from a fixtures cache that we record once and commit. Composer's side hits real Packagist — this is unavoidable and is why the harness is opt-in. CI pins a Packagist mirror to keep behavior reproducible.

### Layer 5 — Derivation / error reporting snapshots  (lands with Phase B, grows in Phase C)

**Where:** `crates/bougie-composer-resolver/src/error.rs`, inline `#[cfg(test)]`.

**What:** for each canonical conflict shape (no-satisfier, transitive conflict, platform req missing, replace conflict, stability conflict, partial-update conflict), construct the failing input as a unit fixture, run the resolver, and assert the rendered error message matches an embedded `const EXPECTED: &str`.

**Choice — no `insta`:** the existing codebase declares `insta` as a dev-dep but does not use it. Following convention, use embedded `const` strings with multi-line raw literals (`r#"..."#`) and `assert_eq!` against the rendered output. Snapshot regeneration is manual (run the test, paste output) — same workflow as `bougie-composer/src/lockfile.rs` hash fixtures.

**Coverage target:** ~8–10 snapshots by end of Phase C. Includes both `why <pkg>` and `why-not <pkg>` output for representative inputs.

**Gates:** every PR.

## Performance test (Phase C only)

`crates/bougie-composer-resolver/benches/resolve.rs` — `criterion` benchmark for `resolve()` on three sizes (small/medium/large fixture from Layer 4). **Not** a regression gate in CI initially (mirrors the AUTOLOADER_PLAN approach — bench infrastructure first, gates only once numbers stabilize). Decision to add a regression gate is deferred until Phase C is near-shipping.

## What we are *not* testing

- **Composer plugin behavior.** Hard scope boundary. If a fixture's lock requires running a plugin to produce its `vendor/`, the fixture is excluded from Layer 4.
- **Composer script execution.** Same. Fixtures with `post-install-cmd` etc. are excluded.
- **VCS / source / artifact repository installs.** Phase D scope; not tested here.
- **`composer.phar` itself.** We assume upstream Composer is correct. When Layer 4 diverges, the bug is on our side until proven otherwise — but if proven otherwise (rare), the fixture moves to a "known divergence" list with a comment, not a "fix Composer" task.
- **Byte equality of `composer.lock`.** Semantic equality only at Layer 4 (per RESOLVER_PLAN.md risk #4). If we later decide byte-equality matters, Layer 3 grows to cover it.
- **Network failure / retry behavior.** That belongs in `bougie-fetch`'s own tests, not the resolver's. Resolver tests use either embedded `MetadataIndex` (Layers 1–3, 5) or live network with a real composer.phar baseline (Layer 4).

## Files touched / added

- **New:** `crates/bougie-semver/src/{version,constraint}.rs` — inline tests + ported semver conformance cases.
- **New:** `crates/bougie-composer-resolver/src/pubgrub/provider.rs` — inline solver unit fixtures.
- **New:** `crates/bougie-composer-resolver/src/lock.rs` — inline lockfile-fidelity tests.
- **New:** `crates/bougie-composer-resolver/src/error.rs` — inline derivation snapshots.
- **New:** `crates/bougie/tests/composer_cross_check.rs` — gated cross-check integration test.
- **New:** `crates/bougie/tests/fixtures/composer_projects/<name>/{composer.json,composer.lock}` × 10 — fixture corpus. (This *is* a new fixtures directory, but only under the existing `crates/bougie/tests/fixtures/` precedent.)
- **New:** `scripts/port-composer-semver-tests.py` — vendor script for Layer 1.
- **New:** `crates/bougie-composer-resolver/benches/resolve.rs` — criterion bench, Phase C only.
- **Updated:** `.github/workflows/ci.yml` — add a job that runs `cargo test --features composer-cross-check` with `composer.phar` installed.
- **Updated:** workspace `Cargo.toml` — add `criterion` to dev-dependencies (Phase C only).

Reuses (no changes needed):

- `crates/bougie-composer/src/lockfile.rs:481` — `content_hash` already tested; we depend on it from Layer 3.
- `crates/bougie/tests/common/mod.rs` — `TestEnv` harness for Layer 4.
- Existing `assert_cmd`, `predicates`, `tempfile`, `wiremock` dev-deps — no new infra.

## Phasing alignment

| Resolver phase | Test layers active |
| --- | --- |
| Phase A (install-from-lock) | Layer 3 (lockfile fidelity) — that's all that's testable; no solver yet |
| Phase B (lock-verify) | Layers 1, 2 (offline-only fixtures), 3, 5 (derivation snapshots on conflict lockfiles) |
| Phase C (full update) | Layers 1, 2 (full ~30), 3, 4 (cross-check), 5 (full), bench |
| Phase D (VCS) | Phase D grows its own fixtures; not scoped here |

## Verification

To validate this plan is implementable as described:

1. **Layer 1 dry-run**: `ls ../composer/tests/Composer/Test/Semver/` exists and the JSON cases are in a portable format. Confirmed previously while reading PhpFileParser; recheck before Phase B kickoff.
2. **Layer 4 dry-run**: pick one project (bougie itself), manually run `composer update` from a wiped lock, check that the lock + vendor are reproducible across two runs on the same machine. If Composer itself is non-deterministic on the input, the cross-check premise fails and Layer 4 needs adjustment.
3. **CI dry-run** before merging Phase C: time the cross-check job. If >10 min, consider moving it to nightly rather than per-PR.

End-to-end signal that the suite is healthy: a deliberate one-line bug in `Range::intersect` should fail Layer 1 (composer/semver conformance), Layer 2 (multiple fixtures), and Layer 4 (most fixtures) — not just one of them. If only one layer catches a planted bug, the suite has a coverage gap.
