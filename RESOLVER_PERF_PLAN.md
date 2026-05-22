# bougie composer-resolver — solve-phase perf plan

Working plan for cutting `bougie composer fetch` / `bougie composer
update` solve-phase time on magento2-class projects. "Solve phase" =
the post-prefetch `pubgrub::resolve` loop driven by `ResolveProvider`
in `crates/bougie-composer-resolver/src/update.rs`.

This is a tuning pass on the existing resolver. No protocol changes,
no behavioral changes that users can observe. Each PR is independently
mergeable, ordered so its win can be measured against a fixed
benchmark before the next one lands.

## Why this, why now

The pre-fetch closure is already multithreaded via `tokio::task::
JoinSet` + `spawn_blocking` (`update.rs:58`, `:1089`). The remaining
slow path is the pubgrub solve itself — single-threaded by design,
called as `&self` on `DependencyProvider`, hot in CPU work that is
re-done across backjumps.

Two issues dominate:

1. `get_dependencies` re-runs `Constraint::parse` + `to_range` for
   every dep on every call (`update.rs:1833`, via
   `push_constraint_map`). pubgrub asks for the same
   `(name, version)` repeatedly during conflict analysis; we re-parse
   every time.
2. `prioritize` returns `()` for every package
   (`update.rs:1851–1860`). Without a priority signal, pubgrub picks
   decision variables in roughly insertion order and backtracks
   orders of magnitude more than it needs to on a magento2-scale
   graph.

Two smaller wins, borrowed from uv's resolver, compound on top:

3. The same `vendor/name` string is cloned into seven maps and into
   every `PubGrubPackage::Package(_)` pubgrub copies around. uv
   solves this with an `Arc<str>`-backed `PackageName` type
   (`arcstr::ArcStr` in `crates/uv-small-str/src/lib.rs:5`).
4. Default `SipHash` is overkill for the string-keyed maps the
   solver hammers. uv uses `rustc_hash::FxHasher` everywhere on the
   hot path (`crates/uv-resolver/src/resolver/index.rs:26`).

What this plan deliberately doesn't touch:

- Multi-threaded solve. pubgrub is single-threaded; there's no
  realistic gain.
- Swapping `RefCell<HashMap>` for `DashMap` / `OnceMap`. uv only
  uses those for the *fetch* layer, which bougie already parallelizes.
- Replacing pubgrub itself.
- `LockPackage` redesign in `bougie-composer`. The cache lives on
  the provider, not on the foreign serde-bound struct.

## PR 0 — magento2 benchmark fixture + criterion harness

Every later PR cites the number from this one. Without it, the
`prioritize` TODO note ("lands when benchmark fixtures exist to
validate the change", `update.rs:1857`) is unactionable, and the
smaller wins in PR 1–3 are just vibes.

Work:

- Capture a real magento2 fixture: snapshot a `composer.json` for a
  recent `magento/project-community-edition` plus the
  `/p2/<name>.json` and `/p2/<name>~dev.json` responses Packagist
  and `repo.magento.com` return for every package in the closure.
- Store under `crates/bougie-composer-resolver/tests/fixtures/
  magento2/` (composer.json + a flat `packagist/<name>.json` tree
  the existing in-process test server can serve — see the pattern
  in `update/tests.rs:101`).
- Add `crates/bougie-composer-resolver/benches/resolve.rs` with
  criterion, building the same `wiremock` server the integration
  tests use and timing only the post-`pre_fetch_closure`
  `resolve(...)` call. The network fan-out is already trusted to be
  parallel and isn't what we're measuring.
- Gate the fixture bytes under a cargo feature (precedent:
  `test-fixtures` in `.cargo/config.toml`) so they don't ship in the
  binary.
- Record the baseline number in the PR description. This is the
  reference point every later PR cites.

Acceptance: `cargo bench -p bougie-composer-resolver --bench resolve`
runs reproducibly and reports a magento2 solve-only time. No
behavior change.

Baseline (this PR — release profile, `criterion 0.5`, `sample_size = 10`,
on the captured magento2 closure, root pinned to
`magento/community-edition 2.4.8`): **~136 ms** (135.02 / 135.94 /
136.90 ms low / median / high across 10 samples). The captured
fixture lives at `crates/bougie-composer-resolver/tests/fixtures/
magento2/packagist-index.json.zst` (~800 KB compressed; 2510 packages
in the closure); regenerate with the `scripts/capture-magento2-*.py`
trio. PR 0 chose 2.4.8 over 2.4.9 because the 2.4.9 closure transitively
requires `magento/composer ^1.10.2` whose stable release hadn't landed
on public Packagist when the snapshot was taken (only `1.10.2-beta4`
was available; with `minimum-stability: stable` that would be filtered
out and the resolve would fail). Run with `--features bench-fixtures`;
the fixture is gated behind that feature so the bytes don't ship in
the binary.

## PR 1 — memoize `get_dependencies`

The `versions_for` path already pays similar memoization
(`update.rs:519`); the comment there records that the prior version
cost 11–14% of CPU on a *Laravel*-sized resolve. magento2's deeper
graph and heavier backjumping make `get_dependencies` the next-worst
offender for the same structural reason: re-parsing constraints we
already parsed.

Work:

- Add a new field on `ResolveProvider`:
  `parsed_deps: RefCell<HashMap<(PubGrubPackage, Version), Arc<Vec<
  (PubGrubPackage, ComposerRange)>>>>`. The value is `Arc` so
  subsequent calls hand back a cheap clone of the parsed vec.
- Refactor the body of `get_dependencies` into a helper that returns
  `Result<Arc<Vec<...>>, ProviderError>`. The outer fn rebuilds
  `Dependencies::Available(DependencyConstraints)` from the slice on
  each call (verify in the PR that `ComposerRange` clone is shallow
  enough to make this worthwhile — it should be; it wraps pubgrub's
  `Ranges`).
- Memoize the virtual-selection branch (`update.rs:1928–1972`) too.
  The per-call `BTreeMap` allocation is small but the branch is hot
  for projects with many PSR-style virtuals.
- Do **not** memoize on `LockPackage` itself. `LockPackage` lives in
  `bougie-composer`, is `Deserialize/Serialize`-bound, and is reused
  outside the resolver. The cache belongs on the provider.
- `LockVerifyProvider` is already pre-parsed at build time
  (`verify/provider.rs:63`) — no change needed there.

Acceptance: PR 0 benchmark moves measurably (target ≥15%, accept
whatever shows up). All `update::tests` pass.

## PR 2 — `FxHashMap` on the solver's hot maps

`rustc-hash` is already a transitive dep (Cargo.lock — it comes in
via pubgrub itself), so adding it directly costs nothing in
build-time or binary-size terms. uv standardizes on `FxHasher` on
every map pubgrub touches and we should too.

Work:

- Add `rustc-hash = "2"` to
  `crates/bougie-composer-resolver/Cargo.toml`.
- Define a local
  `type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;`
  alias in `update.rs` and `metadata.rs` (or a small `hash.rs`
  module the crate re-exports from).
- Swap the seven `RefCell<HashMap<...>>` fields on `ResolveProvider`
  (`update.rs:152–206`), the `parsed_deps` field added in PR 1, and
  the inner `HashMap<String, String>` of `v1_provider_tables`.
- Swap `LockVerifyProvider::locked` / `deps`
  (`verify/provider.rs:61–63`) on the way through — same hot path
  inside pubgrub.
- Leave `BTreeMap`s alone. They're for ordered iteration (used in
  serialization paths) and can't be swapped without changing
  semantics.

Acceptance: every map pubgrub reads through uses `FxHasher`.
Benchmark moves; expect a smaller win than PR 1 (single-digit
percent) but it's nearly free.

## PR 3 — interned `PackageName` (`Arc<str>`-backed)

The same `vendor/name` lives in `cache`, `merged_cache`,
`virtual_providers`, `virtual_selections`, `virtual_wildcards`,
`v1_provider_tables`, the parsed-deps cache from PR 1, *and* in
every `PubGrubPackage::Package(String)` pubgrub clones during
conflict analysis. Owning `String`s everywhere costs allocations on
every clone and inflates the maps' working set.

uv solves this with `PackageName(SmallString(arcstr::ArcStr))`
(`crates/uv-normalize/src/package_name.rs:31`). The `arcstr::ArcStr`
type is a niche-optimized `Arc<str>` — cheap clone, hash-friendly,
zero allocator churn once interned.

Work:

- Add `arcstr = "1"` to `crates/bougie-composer-resolver/Cargo.toml`.
  (Match uv's choice — don't pull in extra SSO unless arcstr already
  provides what we need.)
- New type `PackageName(ArcStr)` in
  `crates/bougie-composer-resolver/src/package_name.rs`. Derive
  `Clone, Eq, PartialEq, Hash, Debug`. Implement `Display`,
  `AsRef<str>`, `From<&str>`, `From<String>`, and the trait bounds
  pubgrub needs on `Self::P`.
- Change `PubGrubPackage::Package(String)` →
  `PubGrubPackage::Package(PackageName)` in
  `verify/provider.rs:28`. This is the focal change — touches
  `update.rs`, `verify.rs`, and the tests under both.
- Swap `ResolveProvider` map keys: `cache`, `merged_cache`,
  `virtual_providers`, `virtual_wildcards`, `v1_provider_tables`
  switch `String` → `PackageName`. `virtual_selections` switches
  `(String, Version)` → `(PackageName, Version)`. `root_deps`
  switches `Vec<(String, ComposerRange)>` →
  `Vec<(PackageName, ComposerRange)>`.
- The `LockPackage` boundary stays `String` (foreign type,
  serde-bound). Intern at the resolver boundary: in
  `load_real_candidates`, `compute_virtual_contributions`, and
  `read_root_requires`, wrap with `PackageName::from(...)` once.
- Mechanical fallout: anywhere we destructure
  `PubGrubPackage::Package(name)` and treated `name` as `&str` keeps
  working via `name.as_ref()`. Anywhere we construct the enum from a
  `String` needs a `PackageName::from`. Expect ~30–50 call sites.

Acceptance: benchmark improves further. All tests pass.
`lock_package_for`, `synthesize_*`, and `register_virtuals_from`
consume `&PackageName` or `&str` rather than owned `String`.

## PR 4 — `prioritize` heuristic

The TODO at `update.rs:1857` is explicit: "Tsai-style 'fewer
candidates first'". This is the single largest expected win on
magento2 — wrong decision order is what makes pubgrub backtrack
deeply, and on a graph with several hundred packages the cost of
wrong-ordering compounds super-linearly. PRs 1–3 make each call
cheaper; PR 4 reduces how many calls happen at all.

Work:

- Set `type Priority = Reverse<u32>` (pubgrub picks the package with
  the *highest* priority; `Reverse` lets us write the logic as
  "fewer-in-range candidates wins").
- In `prioritize(&self, package, range, _stats)`:
  - `Root` → max priority. Trivial, decide first.
  - `Package(name)` → count `versions_for(name)` entries that fall
    in `range`. Fewer = higher priority.
- **Don't fetch from `prioritize`.** It's called constantly. For
  names whose `merged_cache` / `cache` entry is already populated,
  the count is cheap. For names that haven't been touched yet,
  return a default priority instead of triggering a network fetch
  (the pre-fetch closure has already closed by the time
  `prioritize` is being called). Document with a comment matching
  the style of the others in this file.
- Run the full integration test suite. pubgrub's resolution is
  deterministic given fixed inputs, but the priority changes the
  search order — ties in choose_version paths can surface
  differently. Any test that relies on resolution order rather than
  resolution *result* needs a tighter assertion.
- If the PR 0 benchmark regresses on smaller fixtures
  (Laravel-sized), measure: the heuristic should be net-positive at
  every size, but bookkeeping might dominate on tiny graphs.
  Mitigate with a "skip prioritization if total package count < N"
  guard *only* if measured — do not pre-emptively add it.

Acceptance: magento2 solve time drops significantly vs. the PR 0
baseline (the bench tells us how much). No test regressions. The
TODO at `update.rs:1857` is replaced with a real implementation and
a rationale comment.

## Ordering rationale

- PR 0 first because every later PR cites its number.
- PR 1 before PR 3 because the parsed-deps cache wants `PackageName`
  keys eventually but starting with `String` keys is fine — PR 3
  trivially swaps the key type along with everything else. We don't
  want the largest single win gated on PR 3's larger surface area.
- PR 2 (FxHasher) is cheap and fits anywhere; placing it before
  PR 3 means PR 3 — already a "touch everything" PR — doesn't have
  to also touch hashers.
- PR 4 last because it's the riskiest (changes pubgrub's search
  path) and benefits most from the others' optimizations: a faster
  `versions_for` and `get_dependencies` make the heuristic itself
  cheap, since it's called per decision.

## When this plan ships

Per CLAUDE.md convention, delete this file when PR 4 lands. The
finished work lives in git history; the repo root is for current
work.
