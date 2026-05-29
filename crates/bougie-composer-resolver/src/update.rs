//! `bougie composer update`'s pubgrub [`DependencyProvider`]: candidates
//! sourced from Packagist v2 metadata, fetched lazily and cached for
//! the duration of one solve.
//!
//! Parallels [`crate::verify::provider::LockVerifyProvider`] — same
//! `PubGrubPackage` enum, same `to_range` conversion — but instead of
//! a single locked candidate per package, each package has every
//! version Packagist publishes (filtered by stability and the active
//! range).
//!
//! Phase C scope (this PR): just the provider. Resolves from a fresh
//! `composer.json` over the network, prefers the highest matching
//! stable version, drops platform packages (`php`, `ext-*`, etc.) at
//! the require boundary the way `LockVerifyProvider` does.
//!
//! `replace` and `provide` route through a virtual-provider index
//! populated by an eager pre-fetch closure (mirrors Composer's
//! `PoolBuilder::buildPool`). Virtual names — `psr/log-implementation`,
//! `psr/http-client-implementation`, and the rest of the PSR-virtual
//! family — are resolvable even though Packagist returns 404 for
//! them, because some real package in the graph declared
//! `provide: { Q: ... }`. `replace` additionally emits a require
//! from the replacing package on the replaced name, mirroring
//! Composer's "no coexistence" rule; `provide` is virtual-only.
//!
//! `minimum-stability` (top-level composer.json) and the per-package
//! `@<stability>` flag (root require suffix like `"acme/foo":
//! "^1.0@dev"`) are honored — see [`ResolveProvider::versions_for`].
//! `prefer-stable` (top-level composer.json) is honored by
//! [`ResolveProvider::choose_version`]: when enabled and the floor
//! admits non-stable versions, the candidate scan does a two-pass
//! lookup — highest stable in range first, then highest in range
//! regardless of stability.
//!
//! When the effective stability for a package is
//! [`Stability::Dev`], `versions_for` also consults the dev variant
//! `/p2/<name>~dev.json` and merges its entries newest-first with
//! the stable doc. A 404 on the dev variant is absorbed silently
//! (many packages have no branches); other errors propagate.
//!
//! Lockfile writing goes through [`resolve_for_lockfile`], which runs
//! the solver twice (full graph + prod-only) and partitions the
//! difference into Composer's `packages` / `packages-dev` arrays.
//! The CLI shim wraps the outcome in a `Lock` and hands it to
//! `bougie_composer::lockfile::write_lock`.
//!
//! Out of scope (follow-ups):
//! - `prefer-stable` candidate-ordering
//! - Platform-package version checks against the bougie-pinned PHP
//!   (issue #118 — affects polyfill-style `replace: { php: "..." }`
//!   declarations too)
//! - Custom repositories beyond Packagist
//! - Byte-equivalence with Composer's own lockfile output (we
//!   promise semantic equivalence: composer install accepts our
//!   lock; key order and topological sort may diverge)
//!
//! The pre-fetch closure dispatches HTTP via a Semaphore-bounded
//! `tokio::task::JoinSet` so unrelated packages download in parallel
//! (cribbed from uv's `UV_CONCURRENT_DOWNLOADS` model); cap defaults
//! to 50 and is tunable via `BOUGIE_CONCURRENT_FETCHES`. Virtual
//! providers are still registered single-threaded after the fan-out
//! settles so `provide`/`replace` indexing stays deterministic.

use std::cell::{Ref, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::hash::{FxHashMap, FxHashSet};
use crate::package_name::PackageName;

use bougie_composer::lockfile::{LockAutoload, LockPackage};
use bougie_paths::Paths;
use bougie_semver::constraint::Constraint;
use bougie_semver::stability::Stability;
use bougie_semver::version::Version;
use eyre::{eyre, Result, WrapErr};
use pubgrub::{
    resolve, DefaultStringReporter, Dependencies, DependencyConstraints, DependencyProvider,
    PackageResolutionStatistics, PubGrubError, Reporter,
};
use serde::Serialize;
use serde_json::Value;

use crate::metadata::build_client;

/// Whether `solve_into_lock_packages` should render a progress
/// spinner during its pre-fetch closure. `resolve_for_lockfile`
/// drives the spinner during the full-graph solve and hides it
/// for the prod-only solve that follows (cache hits, instant —
/// flashing a fresh spinner would just be noise).
#[derive(Copy, Clone)]
enum ProgressMode {
    Visible,
    Hidden,
}

use crate::metadata::{
    fetch_package_metadata_optional, fetch_package_metadata_optional_async,
    fetch_package_metadata_v1_optional, fetch_package_metadata_v1_optional_async,
    load_v1_provider_table, load_v1_provider_table_async, probe_protocol, Repo,
    RepoProtocol, Variant,
};
use crate::verify::{is_platform, to_range, ComposerRange, ProviderError, PubGrubPackage};

/// pubgrub provider that resolves a fresh `composer.json` against
/// Packagist v2 metadata.
///
/// The metadata cache lives behind `RefCell` because pubgrub's
/// `DependencyProvider` methods take `&self`; we need to mutate the
/// cache on the lazy-fetch path. Single-threaded by construction (one
/// pubgrub solve drives the provider end-to-end), so a `RefCell` is
/// the right tool — `Mutex` would just add overhead.
pub struct ResolveProvider {
    client: reqwest::blocking::Client,
    paths: Paths,
    /// Repositories to consult for package metadata, in declaration
    /// order (highest priority first). Built from composer.json's
    /// `repositories` field plus an implicit public Packagist entry
    /// unless `{ "packagist.org": false }` disables it.
    /// `versions_for(name)` walks this list and takes the first
    /// non-404 hit.
    repos: Vec<Repo>,
    root_deps: Vec<(PackageName, ComposerRange)>,
    root_version: Version,
    /// Composer's top-level `minimum-stability` (`stable` by default).
    /// Sets the floor for candidate stability; any version whose
    /// stability is *below* this is dropped from `versions_for`.
    minimum_stability: Stability,
    /// Composer's top-level `prefer-stable` (`false` by default).
    /// When true, `choose_version` does a two-pass scan: highest
    /// stable in range first, then fall back to the highest
    /// in-range candidate regardless of stability. This is the
    /// difference between "beta 2.0.0-beta1 wins over stable 1.0.0"
    /// and Composer's "pick the highest stable when one fits."
    prefer_stable: bool,
    /// Per-package stability overrides extracted from `@<stability>`
    /// suffixes on root requires (`acme/foo: ^1.0@dev`). Takes
    /// precedence over [`Self::minimum_stability`] for the named
    /// package. Composer only honors these at the root require/
    /// require-dev level; transitive deps don't carry flags.
    stability_flags: HashMap<String, Stability>,
    /// `vendor/name` → versions list, newest-first, filtered by the
    /// effective stability gate. Populated lazily on first
    /// `choose_version` / `get_dependencies` for that package.
    /// Parsed `Version` is kept alongside the raw `LockPackage` so
    /// `choose_version` and `get_dependencies` don't need to
    /// re-parse `LockPackage::version` and risk normalization
    /// differences between our parser and Packagist's
    /// `version_normalized`. Only real Packagist candidates are
    /// cached here — virtual candidates are merged in on every
    /// `versions_for` call so a late provider registration becomes
    /// visible (the pre-fetch queue can visit a virtual name before
    /// its provider is loaded).
    cache: RefCell<FxHashMap<PackageName, Vec<(Version, LockPackage)>>>,
    /// Memoized merge of [`Self::cache`] + virtuals, sorted +
    /// deduplicated. This is the shape pubgrub actually consumes via
    /// [`Self::versions_for`]; populating it lazily on first lookup
    /// and borrowing it on subsequent ones is the main reason
    /// `versions_for` can hand pubgrub a `Ref<[..]>` instead of
    /// cloning the cache vec on every probe. Pre-fetch closes before
    /// pubgrub runs and `register_virtuals_from` only runs there, so
    /// the virtual index is stable from this cache's perspective —
    /// invalidation isn't needed.
    merged_cache: RefCell<FxHashMap<PackageName, Vec<(Version, LockPackage)>>>,
    /// Index of virtual provider entries: maps each virtual package
    /// name (e.g. `psr/http-client-implementation`) to the list of
    /// real packages that provide or replace it. Populated by
    /// [`Self::pre_fetch_closure`] before the solve runs — matches
    /// what Composer's `PoolBuilder` does, so that when pubgrub asks
    /// `choose_version("psr/http-client-implementation", ^1)` the
    /// answer can come from the index even though Packagist has no
    /// `/p2/psr/http-client-implementation.json`.
    virtual_providers: RefCell<FxHashMap<PackageName, Vec<VirtualProvider>>>,
    /// Wildcard / range-shaped replace+provide clauses. Composer's
    /// `replace: { codeception/phpunit-wrapper: "*" }` on
    /// codeception 5.x says "I replace any version of
    /// phpunit-wrapper" — Composer's `whatProvides` then satisfies
    /// any require on phpunit-wrapper from codeception's pool entry.
    /// We model this by remembering the range the provider declares
    /// and synthesizing a candidate inside the consumer's range at
    /// `choose_version` time (when the consumer's range is known).
    virtual_wildcards: RefCell<FxHashMap<PackageName, Vec<WildcardProvider>>>,
    /// Reverse-lookup: for each (`virtual_name`, `selected_version`),
    /// every (`provider_name`, `provider_version`) pair that registered
    /// it. Used by `get_dependencies` to pin the providing real
    /// package(s).
    ///
    /// A virtual is often registered by more than one provider at
    /// the same version: e.g. several versions of one library that
    /// each `replace: { renamed/pkg: 1.0.0 }` to keep the renamed
    /// virtual stable across the library's own version bumps. When
    /// every registration shares the same provider name, the pin
    /// emitted by `get_dependencies` is the union of those provider
    /// versions so pubgrub can pick whichever fits the surrounding
    /// constraints. When distinct provider packages compete (the
    /// classic "multiple PSR implementations" case), we fall back
    /// to the first registration — pubgrub has no OR for the
    /// constraints `get_dependencies` returns, so emitting all of
    /// them would over-constrain.
    virtual_selections: RefCell<FxHashMap<(PackageName, Version), Vec<(PackageName, Version)>>>,
    /// Per-v1-repo merged provider lookup tables (package name →
    /// sha256). Lazily populated on the first per-package lookup
    /// against a given v1 repo, keyed by `repo.url`. Composer v1
    /// requires loading every `provider-includes` file before any
    /// package can be resolved (the includes are the only index
    /// telling us which package's hash to use); this cache makes
    /// that load happen at most once per resolve per repo.
    v1_provider_tables: RefCell<FxHashMap<String, FxHashMap<String, String>>>,
    /// Memoized `get_dependencies` output. pubgrub asks for the same
    /// `(package, version)` pair repeatedly during conflict analysis;
    /// without this cache we re-run `Constraint::parse` + `to_range`
    /// for every require + replace clause on every call. Storing the
    /// parsed result as `Arc<Vec<_>>` lets cache hits hand pubgrub a
    /// shallow `Arc::clone` and walk the cached slice straight into a
    /// fresh `DependencyConstraints` map. Covers both real packages
    /// and the virtual-selection branch; `Root` skips the cache (its
    /// deps come pre-parsed from `root_deps` and pubgrub only asks
    /// for them once).
    ///
    /// Mirror of the `merged_cache` pattern (which already pays the
    /// same memoization toll on `versions_for`). The `versions_for`
    /// note records that the prior un-memoized version cost 11–14%
    /// of CPU on a Laravel-sized resolve; magento2's deeper graph
    /// makes constraint re-parsing the next-worst hot spot for the
    /// same structural reason.
    parsed_deps: RefCell<
        FxHashMap<(PubGrubPackage, Version), Arc<Vec<(PubGrubPackage, ComposerRange)>>>,
    >,
    /// Spinner ticked on every `versions_for` call so the pubgrub
    /// `resolve` phase has visible progress. Defaults to hidden;
    /// orchestrators flip it on with `begin_solve_progress` around
    /// the `resolve` call and clear it with `finish_solve_progress`.
    /// Without this the solver phase is silent — for projects with
    /// hundreds of dependencies that silence reads as a hang.
    solve_progress: RefCell<SolveProgress>,
    /// Versions excluded by post-solve conflict validation. When the
    /// solver picks version V for package P and V's conflict map is
    /// violated by another package in the solution, (P, V) is added
    /// here and the solve retries. `versions_for` filters these out.
    conflict_excludes: RefCell<FxHashSet<(PackageName, Version)>>,
    raw_root_constraints: FxHashMap<String, String>,
}

/// One entry in the virtual provider index — "real package
/// `provider_name@provider_version` declares it provides/replaces
/// the virtual name at version `provided_version`."
#[derive(Debug, Clone)]
pub struct VirtualProvider {
    pub provider_name: PackageName,
    pub provider_version: Version,
    pub provided_version: Version,
}

/// Wildcard provider entry — "real package
/// `provider_name@provider_version` declares it
/// provides/replaces the virtual name across the range
/// `provided_range`." Used for `*` and other range-shaped
/// `replace` / `provide` clauses where no single version is
/// authoritative. The synthetic candidate is picked from inside
/// the consumer's required range at `choose_version` time.
#[derive(Debug, Clone)]
pub struct WildcardProvider {
    pub provider_name: PackageName,
    pub provider_version: Version,
    /// The range the provider declared it covers. For `*` this is
    /// `Ranges::full()` (matches every consumer constraint). For
    /// something like `replace: { Q: "^1.0" }` we use the parsed
    /// constraint's range.
    pub provided_range: ComposerRange,
}

/// Pre-parsed contributions one package's `provide`/`replace` clauses
/// make to the virtual-provider indexes. Pure data — no `&self` —
/// safe to compute inside a `spawn_blocking` worker. The expensive
/// work (`Version::parse` / `Constraint::parse` / `to_range`) happens
/// during the network-bound fetch phase; the main thread later
/// applies these in deterministic order via
/// [`ResolveProvider::apply_virtual_contributions`], which is just
/// a handful of `HashMap::entry().or_default().push(_)` calls.
#[derive(Default)]
struct VirtualContributions {
    /// Specific-version clauses (`replace: { foo: "1.2.3" }` or
    /// `provide: { bar: self.version }`). Land in
    /// `virtual_providers` + `virtual_selections`.
    exacts: Vec<ExactContribution>,
    /// Range / wildcard clauses (`replace: { foo: "*" }`,
    /// `provide: { foo: "^1.0" }`). Land in `virtual_wildcards`.
    wildcards: Vec<WildcardContribution>,
}

struct ExactContribution {
    virtual_name: PackageName,
    provider_name: PackageName,
    provider_version: Version,
    provided_version: Version,
}

struct WildcardContribution {
    virtual_name: PackageName,
    provider_name: PackageName,
    provider_version: Version,
    provided_range: ComposerRange,
}

/// Pure counterpart to [`ResolveProvider::register_virtuals_from`]:
/// walks one package's versions and decodes every `provide`/`replace`
/// clause into a [`VirtualContributions`] payload. No `&self`, no
/// `RefCell` access — safe to call from a worker thread. The
/// expensive cost here is the `Version::parse` / `Constraint::parse`
/// per clause; on a magento2-sized resolve that totals ~150 ms of
/// previously-serial main-thread work.
fn compute_virtual_contributions(
    provider_name: &PackageName,
    versions: &[LockPackage],
) -> VirtualContributions {
    let mut out = VirtualContributions::default();
    for p in versions {
        let Ok(provider_version) = Version::parse(&p.version) else {
            continue;
        };
        for clause_map in [&p.replace, &p.provide] {
            for (virtual_name, raw_constraint) in clause_map {
                if is_platform(virtual_name) {
                    continue;
                }
                let effective = if raw_constraint == "self.version" {
                    &p.version
                } else {
                    raw_constraint
                };
                if let Ok(v) = Version::parse(effective) {
                    out.exacts.push(ExactContribution {
                        virtual_name: PackageName::from(virtual_name.as_str()),
                        provider_name: provider_name.clone(),
                        provider_version: provider_version.clone(),
                        provided_version: v,
                    });
                } else if let Ok(constraint) = Constraint::parse(effective) {
                    out.wildcards.push(WildcardContribution {
                        virtual_name: PackageName::from(virtual_name.as_str()),
                        provider_name: provider_name.clone(),
                        provider_version: provider_version.clone(),
                        provided_range: to_range(&constraint),
                    });
                }
                // Unparseable as either Version or Constraint: skip
                // (same fail-soft behavior as the original).
            }
        }
    }
    out
}

impl std::fmt::Debug for ResolveProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolveProvider")
            .field(
                "repos",
                &self.repos.iter().map(|r| &r.url).collect::<Vec<_>>(),
            )
            .field("root_deps", &self.root_deps)
            .field("cache_keys", &self.cache.borrow().keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ResolveProvider {
    /// Build from the parsed `composer.json` (top-level `Value`),
    /// reading `require`, optionally `require-dev`, and the
    /// `repositories` array. `default_packagist` is the implicit
    /// public Packagist repo (typically `Repo::packagist()`);
    /// composer.json's `{ "packagist.org": false }` entry disables
    /// the implicit append. Network access is deferred until pubgrub
    /// asks for a candidate.
    pub fn build(
        client: reqwest::blocking::Client,
        paths: Paths,
        default_packagist: Repo,
        composer_json: &Value,
        no_dev: bool,
    ) -> Result<Self, BuildError> {
        // Pre-built auth from composer.json's `config` section (and,
        // when called via the orchestrator, also from a project-
        // root `auth.json`). `build()` itself doesn't touch disk —
        // the caller assembles the map.
        Self::build_with_auth(
            client,
            paths,
            default_packagist,
            composer_json,
            no_dev,
            HashMap::new(),
        )
    }

    /// Like [`build`] but accepts a pre-assembled auth map keyed by
    /// host. The orchestrator (`dry_run_update` / `resolve_for_lockfile`)
    /// reads composer.json's `config.http-basic` / `config.bearer`
    /// and any project-level `auth.json` before calling.
    pub fn build_with_auth(
        client: reqwest::blocking::Client,
        paths: Paths,
        default_packagist: Repo,
        composer_json: &Value,
        no_dev: bool,
        auth: HashMap<String, crate::metadata::AuthCredentials>,
    ) -> Result<Self, BuildError> {
        let minimum_stability = read_minimum_stability(composer_json)?;
        let prefer_stable = read_prefer_stable(composer_json)?;
        let (root_deps, stability_flags, raw_root_constraints) =
            read_root_requires(composer_json, no_dev)?;
        let repos = read_repositories(composer_json, default_packagist, &auth)?;
        // Any synthetic value works for the root version — pubgrub
        // never compares it against another candidate. Match the
        // verify provider's choice for cross-module consistency.
        let root_version =
            Version::parse("0.0.0.0").map_err(|e| BuildError::Internal(e.to_string()))?;
        Ok(Self {
            client,
            paths,
            repos,
            root_deps,
            root_version,
            minimum_stability,
            prefer_stable,
            stability_flags,
            cache: RefCell::new(FxHashMap::default()),
            merged_cache: RefCell::new(FxHashMap::default()),
            virtual_providers: RefCell::new(FxHashMap::default()),
            virtual_wildcards: RefCell::new(FxHashMap::default()),
            virtual_selections: RefCell::new(FxHashMap::default()),
            v1_provider_tables: RefCell::new(FxHashMap::default()),
            parsed_deps: RefCell::new(FxHashMap::default()),
            solve_progress: RefCell::new(SolveProgress::hidden()),
            conflict_excludes: RefCell::new(FxHashSet::default()),
            raw_root_constraints,
        })
    }

    /// Begin rendering the solve-phase progress spinner. Call after
    /// `pre_fetch_closure` (whose own spinner has already finished)
    /// and before `pubgrub::resolve`.
    pub fn begin_solve_progress(&self) {
        *self.solve_progress.borrow_mut() = SolveProgress::new();
    }

    /// Finish and clear the solve-phase progress spinner. Safe to
    /// call when the spinner is already hidden.
    pub fn finish_solve_progress(&self) {
        self.solve_progress.borrow().finish();
        *self.solve_progress.borrow_mut() = SolveProgress::hidden();
    }

    /// The synthetic root version pubgrub should `resolve` against.
    pub fn root_version(&self) -> Version {
        self.root_version.clone()
    }

    /// Clear the memoized `parsed_deps` cache. Must be called before
    /// re-solving so that any changed `conflict_excludes` take effect
    /// (the cached deps still contain the old version's constraints).
    fn reset_for_re_solve(&self) {
        self.parsed_deps.borrow_mut().clear();
    }

    /// Check the solution for conflict violations. Returns the set of
    /// `(declarer, version)` pairs whose conflict map is violated by
    /// another package in the solution.
    fn check_conflict_violations(
        &self,
        solution: &[(&PubGrubPackage, &Version)],
    ) -> Vec<(PackageName, Version)> {
        let solution_map: FxHashMap<&str, &Version> = solution
            .iter()
            .filter_map(|(pkg, ver)| match pkg {
                PubGrubPackage::Package(n) => Some((n.as_str(), *ver)),
                PubGrubPackage::Root => None,
            })
            .collect();

        let mc = self.merged_cache.borrow();
        let mut violations = Vec::new();
        for (pkg, version) in solution {
            let PubGrubPackage::Package(name) = pkg else { continue };
            let Some(cached) = mc.get(name.as_str()) else { continue };
            let Some((_, entry)) = cached.iter().find(|(v, _)| v == *version) else {
                continue;
            };
            for (conflict_name, raw_constraint) in &entry.conflict {
                if is_platform(conflict_name) {
                    continue;
                }
                let Some(target_ver) = solution_map.get(conflict_name.as_str()) else {
                    continue;
                };
                let effective = if raw_constraint == "self.version" {
                    &entry.version
                } else {
                    raw_constraint.as_str()
                };
                let (cleaned, _) = split_stability_flag(effective);
                let Ok(constraint) = Constraint::parse(cleaned) else {
                    continue;
                };
                let range = to_range(&constraint);
                if range.contains(target_ver) {
                    violations.push((name.clone(), (*version).clone()));
                    break;
                }
            }
        }
        violations
    }

    /// Probe each configured repository's `packages.json` and record
    /// the discovered Composer protocol (v2 with `/p2/` direct
    /// fetches, or v1 with `provider-includes` + `providers-url`) on
    /// the [`Repo`] itself. The per-package fetcher dispatches on
    /// that stored protocol; without this step every repo is treated
    /// as v2 and v1 repos (like `repo.magento.com`) crash on a
    /// 302→HTML response.
    ///
    /// Probe failures (network blip, malformed `packages.json`) leave
    /// the protocol as `None` — the fetcher then falls back to the v2
    /// `/p2/` path with the defensive HTML-body guard. That gives a
    /// transient hiccup a chance to be a no-op rather than silently
    /// de-listing a working repo.
    ///
    /// Idempotent and cheap to re-run; call once after
    /// `build_with_auth` and before any `pre_fetch_closure` /
    /// `resolve` so the probe runs before any per-package traffic.
    pub fn discover_repos(&mut self) {
        let original = std::mem::take(&mut self.repos);
        let mut updated = Vec::with_capacity(original.len());
        for repo in original {
            let protocol = probe_protocol(&self.client, &repo).ok();
            updated.push(repo.with_protocol(protocol));
        }
        self.repos = updated;
    }

    /// Inspect what's been fetched so far. Exposed for tests + future
    /// debug verbs. Counts both the pre-fetch staging cache and the
    /// post-merge cache `versions_for` drains into, so a package
    /// counts once regardless of whether pubgrub has consulted it yet.
    pub fn cache_size(&self) -> usize {
        self.cache.borrow().len() + self.merged_cache.borrow().len()
    }

    /// Return the cached [`LockPackage`] for `(name, version)` if it
    /// was loaded during the current solve. Used by the lockfile
    /// writer to recover the full Packagist entry (dist, require,
    /// autoload, etc.) for each resolved version. Looks in the
    /// post-merge cache because `versions_for` moves entries out of
    /// the staging cache on first probe.
    pub fn lock_package_for(&self, name: &str, version: &Version) -> Option<LockPackage> {
        let cache = self.merged_cache.borrow();
        let entries = cache.get(name)?;
        entries
            .iter()
            .find(|(v, _)| v == version)
            .map(|(_, p)| p.clone())
    }

    /// After a `NoSolution` from pubgrub, check each root requirement
    /// against the metadata cache to find ALL independent problems.
    /// Returns a multi-problem error string if problems are found,
    /// or `None` to fall back to the pubgrub derivation tree.
    fn analyze_resolution_problems(&self) -> Option<String> {
        let mut problems: Vec<String> = Vec::new();
        let mut unsatisfiable: FxHashSet<String> = FxHashSet::default();

        self.collect_missing_version_problems(&mut problems, &mut unsatisfiable);
        self.collect_transitive_conflict_problems(&mut problems, &unsatisfiable);

        if problems.is_empty() {
            return None;
        }
        Some(problems.join("\n"))
    }

    fn raw_constraint_for(&self, name: &str) -> &str {
        self.raw_root_constraints
            .get(name)
            .map_or("*", String::as_str)
    }

    fn collect_missing_version_problems(
        &self,
        problems: &mut Vec<String>,
        unsatisfiable: &mut FxHashSet<String>,
    ) {
        for (name, range) in &self.root_deps {
            let versions = self.peek_cached_versions(name.as_str());
            let has_match = versions
                .as_ref()
                .is_some_and(|vs| vs.iter().any(|(v, _)| range.contains(v)));
            if has_match {
                continue;
            }
            unsatisfiable.insert(name.to_string());
            let constraint = self.raw_constraint_for(name.as_str());

            match &versions {
                Some(vs) if !vs.is_empty() => {
                    let available =
                        format_version_list(vs.iter().map(|(v, _)| v.to_string()));
                    problems.push(format!(
                        "  Problem {}\n    \
                         - Root composer.json requires {name} {constraint}, \
                         found {name}[{available}] but these do not match the constraint.",
                        problems.len() + 1,
                    ));
                }
                _ => {
                    problems.push(format!(
                        "  Problem {}\n    \
                         - Root composer.json requires {name} {constraint}, \
                         but the package was not found in any configured repository.",
                        problems.len() + 1,
                    ));
                }
            }
        }
    }

    fn collect_transitive_conflict_problems(
        &self,
        problems: &mut Vec<String>,
        unsatisfiable: &FxHashSet<String>,
    ) {
        let root_map: FxHashMap<&str, &ComposerRange> = self
            .root_deps
            .iter()
            .map(|(n, r)| (n.as_str(), r))
            .collect();

        for (name, range) in &self.root_deps {
            if unsatisfiable.contains(name.as_str()) {
                continue;
            }
            let Some(versions) = self.peek_cached_versions(name.as_str()) else {
                continue;
            };
            let Some((version, entry)) = versions.iter().find(|(v, _)| range.contains(v))
            else {
                continue;
            };
            for (dep_name, dep_constraint_str) in &entry.require {
                if is_platform(dep_name) {
                    continue;
                }
                let Some(&root_range) = root_map.get(dep_name.as_str()) else {
                    continue;
                };
                let Ok(dep_constraint) = Constraint::parse(dep_constraint_str) else {
                    continue;
                };
                let dep_range = to_range(&dep_constraint);

                let dep_versions = self.peek_cached_versions(dep_name);
                let both_satisfied = dep_versions.as_ref().is_some_and(|dvs| {
                    dvs.iter()
                        .any(|(v, _)| root_range.contains(v) && dep_range.contains(v))
                });
                if both_satisfied {
                    continue;
                }

                let constraint_str = self.raw_constraint_for(name.as_str());
                let root_dep_constraint = self.raw_constraint_for(dep_name);
                let dep_available = dep_versions.as_ref().map_or_else(String::new, |dvs| {
                    format_version_list(
                        dvs.iter()
                            .filter(|(v, _)| dep_range.contains(v))
                            .map(|(v, _)| v.to_string()),
                    )
                });

                problems.push(format!(
                    "  Problem {}\n    \
                     - Root composer.json requires {name} {constraint_str} \
                     -> satisfiable by {name}[{version}].\n    \
                     - {name} {version} requires {dep_name} {dep_constraint_str} \
                     -> found {dep_name}[{dep_available}] but it conflicts \
                     with your root composer.json require ({root_dep_constraint}).",
                    problems.len() + 1,
                ));
            }
        }
    }

    /// Read-only peek into the version caches (merged first, then
    /// staging). Returns `None` when the package isn't cached at all,
    /// `Some(vec)` otherwise (vec may be empty). Unlike `versions_for`,
    /// this never fetches from the network or mutates caches — safe to
    /// call on the error path.
    fn peek_cached_versions(&self, name: &str) -> Option<Vec<(Version, LockPackage)>> {
        if let Some(vs) = self.merged_cache.borrow().get(name) {
            return Some(vs.clone());
        }
        self.cache.borrow().get(name).cloned()
    }

    /// Effective stability floor for `name`: per-package `@<flag>`
    /// at root require time overrides the top-level
    /// `minimum-stability`.
    fn effective_stability(&self, name: &str) -> Stability {
        self.stability_flags
            .get(name)
            .copied()
            .unwrap_or(self.minimum_stability)
    }

    /// Fetch (or read from cache) the version list for one package,
    /// filtered to candidates whose stability is at or above the
    /// effective gate for that package. Versions whose version-string
    /// fails to parse are dropped.
    ///
    /// When the effective stability is [`Stability::Dev`] we also
    /// consult `/p2/<name>~dev.json` (the branch document). Packagist
    /// serves stable numeric versions and branch entries
    /// (`dev-main`, `1.x-dev`, …) in separate documents; without
    /// fetching both, a project with `minimum-stability: dev` can't
    /// pick up branches. Many packages have no branches, in which
    /// case `~dev.json` 404s — we treat that as "no dev candidates"
    /// rather than a hard error.
    ///
    /// The combined list is sorted version-descending so pubgrub's
    /// "first in range" candidate selection still picks the highest
    /// matching version regardless of which document supplied it.
    fn versions_for(
        &self,
        name: &str,
    ) -> Result<Ref<'_, Vec<(Version, LockPackage)>>, ProviderError> {
        self.solve_progress.borrow().tick(name);
        tracing::trace!(package = %name, "versions_for");

        // Fast path: post-merge form already memoized. Hands pubgrub
        // a borrow of the cached vec; previously this method cloned
        // a `Vec<(Version, LockPackage)>` (with full BTreeMap + Value
        // trees per entry) on every `choose_version` /
        // `get_dependencies` probe. The clone showed up as ~11–14% of
        // total CPU on a Laravel-sized resolve, with a matching slug
        // of `drop_in_place<LockPackage>` cost on top — see commit
        // body for the profiler numbers.
        if self.merged_cache.borrow().contains_key(name) {
            return Ok(Ref::map(self.merged_cache.borrow(), |m| {
                m.get(name).expect("present check just succeeded")
            }));
        }

        // Slow path: compute the merged-with-virtuals form once and
        // stash it for the next probe of the same name. We MOVE the
        // entry out of `cache` (the prefetch staging area) into
        // `merged_cache` — no clone — because every reachable
        // package is consulted at most once for its real-candidate
        // list. `lock_package_for` reads from `merged_cache` so the
        // entry is still reachable after this drain.
        let floor = self.effective_stability(name);
        let real_candidates = if let Some(v) = self.cache.borrow_mut().remove(name) {
            v
        } else {
            self.load_real_candidates(name, floor)?
        };

        let mut merged: Vec<(Version, LockPackage)> = real_candidates;
        let real_versions: std::collections::HashSet<&Version> =
            merged.iter().map(|(v, _)| v).collect();
        let virtual_candidates = self.synthesize_virtual_candidates(name, floor);
        // When a real package and a virtual candidate exist at the
        // same version, the dedup below keeps the real entry. Remove
        // the corresponding `virtual_selections` entries so
        // downstream code (get_dependencies, post-solve filter)
        // doesn't mistakenly treat the real package as virtual.
        let stale: Vec<Version> = virtual_candidates
            .iter()
            .filter(|(v, _)| real_versions.contains(v))
            .map(|(v, _)| v.clone())
            .collect();
        if !stale.is_empty() {
            let interned = PackageName::from(name);
            let mut sel = self.virtual_selections.borrow_mut();
            for v in &stale {
                sel.remove(&(interned.clone(), v.clone()));
            }
        }
        merged.extend(virtual_candidates);
        merged.sort_by(|a, b| b.0.cmp(&a.0));
        // Deduplicate consecutive entries with the same parsed
        // version — when both Packagist and a virtual provider
        // register the same name+version, prefer the real entry
        // (sort is stable; real Packagist entries are at the front
        // because they were extended in first).
        merged.dedup_by(|a, b| a.0 == b.0);
        // One `PackageName` intern per cache miss — amortized over
        // every subsequent `versions_for(name)` probe, every
        // `PubGrubPackage::Package(name.clone())` pubgrub allocates,
        // and the lock-package lookup path through `lock_package_for`.
        self.merged_cache
            .borrow_mut()
            .insert(PackageName::from(name), merged);
        Ok(Ref::map(self.merged_cache.borrow(), |m| {
            m.get(name).expect("just inserted")
        }))
    }

    /// Fetch + parse + filter the real-package candidate list by
    /// walking every configured repo and unioning the per-repo
    /// version listings. Side-effects: every loaded version's
    /// `provide` / `replace` declarations get registered in the
    /// virtual-provider index. Returns the filtered parsed entries
    /// (no virtual candidates merged — that happens in
    /// `versions_for`).
    ///
    /// Why union rather than Composer's strict canonical (first
    /// repo wins): in real Magento-style projects a broad mirror
    /// repo often lists a stale slice of a package (e.g. phpstan up
    /// to 1.9.9) while a later, specific repo serves the
    /// requested newer range (2.1.x). Composer continues past the
    /// broad mirror because its canonical filter is constraint-
    /// aware ("first repo with a *matching* version"). Bougie's
    /// version-selection lives in pubgrub's `choose_version` and
    /// doesn't have access to the active range here, so we keep
    /// every repo's candidates and let `choose_version` pick the
    /// highest in range. The trade-off vs. strict canonical: when
    /// two repos both host a matching version, bougie picks the
    /// highest rather than the earlier repo's — close enough for
    /// the cases we've seen.
    fn load_real_candidates(
        &self,
        name: &str,
        floor: Stability,
    ) -> Result<Vec<(Version, LockPackage)>, ProviderError> {
        let mut versions: Vec<LockPackage> = Vec::new();
        for repo in &self.repos {
            let stable_md = self
                .fetch_one(repo, name, Variant::Stable)
                .map_err(|e| {
                    ProviderError(format!(
                        "fetching metadata for {name} from {}: {e:#}",
                        repo.url,
                    ))
                })?;
            let Some(md) = stable_md else { continue };
            if let Some(entries) = md.packages.get(name) {
                versions.extend(entries.iter().cloned());
            }
            if floor == Stability::Dev {
                let dev_md = self
                    .fetch_one(repo, name, Variant::Dev)
                    .map_err(|e| {
                        ProviderError(format!(
                            "fetching dev metadata for {name} from {}: {e:#}",
                            repo.url,
                        ))
                    })?;
                if let Some(md) = dev_md
                    && let Some(extra) = md.packages.get(name) {
                        versions.extend(extra.iter().cloned());
                    }
            }
        }

        // Register virtuals as a side effect of loading. Intern the
        // name once here; `register_virtuals_from` clones it (refcount
        // bump) into every `VirtualProvider` / `WildcardProvider` /
        // `virtual_selections` entry it emits.
        let interned = PackageName::from(name);
        self.register_virtuals_from(&interned, &versions);

        // Filter by effective stability, drop unparseable versions,
        // keep the parsed `Version` alongside the raw `LockPackage`.
        let mut out: Vec<(Version, LockPackage)> = versions
            .into_iter()
            .filter_map(|p| {
                Version::parse(&p.version)
                    .ok()
                    .filter(|v| v.stability() >= floor)
                    .map(|v| (v, p))
            })
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(out)
    }

    /// Protocol-dispatching fetch for one (repo, package, variant)
    /// triple. Hides the v1 vs v2 split from `load_real_candidates`:
    ///
    /// - **v2** (or unknown protocol — typical for Packagist and any
    ///   repo whose probe failed): direct `/p2/<name>.json` GET.
    /// - **v1**: lazily load the repo's merged provider lookup table
    ///   (once per resolve, cached on the provider), look up the
    ///   package's sha256, and fetch the per-package file. v1 has no
    ///   `~dev.json` equivalent — branch versions are listed in the
    ///   same per-package document — so `Variant::Dev` returns
    ///   `Ok(None)` to avoid a second wasted request.
    fn fetch_one(
        &self,
        repo: &Repo,
        package: &str,
        variant: Variant,
    ) -> eyre::Result<Option<bougie_composer::metadata::PackageMetadata>> {
        match &repo.protocol {
            Some(RepoProtocol::V1(discovery)) => {
                if variant == Variant::Dev {
                    // v1 stuffs branches into the single per-package
                    // document — `Variant::Stable` already returned
                    // everything. Save the round-trip.
                    return Ok(None);
                }
                if !self.v1_provider_tables.borrow().contains_key(&repo.url) {
                    let table = load_v1_provider_table(
                        &self.client,
                        &self.paths,
                        repo,
                        discovery,
                    )?;
                    self.v1_provider_tables
                        .borrow_mut()
                        .insert(repo.url.clone(), table);
                }
                let tables = self.v1_provider_tables.borrow();
                let table = tables.get(&repo.url).expect("just inserted");
                fetch_package_metadata_v1_optional(
                    &self.client,
                    &self.paths,
                    repo,
                    discovery,
                    table,
                    package,
                )
            }
            _ => fetch_package_metadata_optional(
                &self.client,
                &self.paths,
                repo,
                package,
                variant,
            ),
        }
    }

    /// Apply pre-computed [`VirtualContributions`] (built by
    /// [`compute_virtual_contributions`] inside the prefetch worker)
    /// to the virtual-provider indexes. Holds all three index
    /// `RefCell`s briefly to do hashmap inserts only — none of the
    /// expensive parsing work happens here. Callers must drive this
    /// in name-sorted order so `virtual_selections.or_default()`
    /// keeps first-write-wins determinism across runs.
    fn apply_virtual_contributions(&self, contributions: VirtualContributions) {
        let mut index = self.virtual_providers.borrow_mut();
        let mut wildcards = self.virtual_wildcards.borrow_mut();
        let mut selections = self.virtual_selections.borrow_mut();
        for e in contributions.exacts {
            let ExactContribution {
                virtual_name,
                provider_name,
                provider_version,
                provided_version,
            } = e;
            index.entry(virtual_name.clone()).or_default().push(VirtualProvider {
                provider_name: provider_name.clone(),
                provider_version: provider_version.clone(),
                provided_version: provided_version.clone(),
            });
            selections
                .entry((virtual_name, provided_version))
                .or_default()
                .push((provider_name, provider_version));
        }
        for w in contributions.wildcards {
            let WildcardContribution {
                virtual_name,
                provider_name,
                provider_version,
                provided_range,
            } = w;
            wildcards.entry(virtual_name).or_default().push(WildcardProvider {
                provider_name,
                provider_version,
                provided_range,
            });
        }
    }

    /// Walk a freshly-loaded package's versions and register every
    /// `provide` / `replace` entry in the virtual-provider index.
    /// `provider_name` is the name we just loaded metadata for.
    /// Platform names (`php`, `ext-*`, ...) are skipped — they're
    /// filtered before reaching pubgrub anyway (issue #118).
    ///
    /// Used by the lazy fallback in [`Self::load_real_candidates`]
    /// (pubgrub asked about a name that wasn't pre-fetched); the
    /// prefetch hot path computes contributions in worker tasks and
    /// applies them via [`Self::apply_virtual_contributions`] for
    /// the wall-clock savings.
    fn register_virtuals_from(&self, provider_name: &PackageName, versions: &[LockPackage]) {
        let mut index = self.virtual_providers.borrow_mut();
        let mut wildcards = self.virtual_wildcards.borrow_mut();
        let mut selections = self.virtual_selections.borrow_mut();
        for p in versions {
            let Ok(provider_version) = Version::parse(&p.version) else {
                continue;
            };
            // Both `provide` and `replace` route through here. The
            // semantic distinction Composer draws ("replace = no
            // coexistence, provide = capability declaration") is
            // approximated below: replace also adds a hard require
            // on the replaced name in `get_dependencies`; this index
            // alone gives provide-style behavior.
            for clause_map in [&p.replace, &p.provide] {
                for (virtual_name, raw_constraint) in clause_map {
                    if is_platform(virtual_name) {
                        continue;
                    }
                    let effective = if raw_constraint == "self.version" {
                        &p.version
                    } else {
                        raw_constraint
                    };
                    // Composer's `replace`/`provide` accepts an
                    // exact version (`"1.0.0"`), a range
                    // (`"^1.12"`), or a wildcard (`"*"`). The three
                    // shapes need different pubgrub encodings:
                    //
                    // 1. Bare version → register a specific-version
                    //    virtual candidate at exactly that version
                    //    (`virtual_providers` index).
                    // 2. Anything else that parses as a Constraint
                    //    (incl. `*`, `^X.Y`, `>=X`, ...) → register
                    //    a wildcard provider with the constraint's
                    //    range. The synthetic candidate is built
                    //    lazily inside `choose_version` once the
                    //    consumer's required range is known.
                    //
                    // This split matters in practice: codeception
                    // 5.x declares `replace: { phpunit-wrapper: "*" }`,
                    // and lib-asserts 2.x requires `phpunit-wrapper
                    // ^7.7.1 | ^8.0.3 | ^9.0`. A specific-version
                    // synthetic at codeception's own version (5.3.5)
                    // wouldn't fit any of those caret ranges and the
                    // resolver would dead-end into the real
                    // phpunit-wrapper's incompatible deps. The
                    // wildcard path picks a version *in the
                    // consumer's range* instead.
                    let interned_virtual = PackageName::from(virtual_name.as_str());
                    if let Ok(v) = Version::parse(effective) {
                        index
                            .entry(interned_virtual.clone())
                            .or_default()
                            .push(VirtualProvider {
                                provider_name: provider_name.clone(),
                                provider_version: provider_version.clone(),
                                provided_version: v.clone(),
                            });
                        // Map the (virtual, provided) tuple back to
                        // every provider that registered it.
                        // `get_dependencies` later groups by
                        // provider name and emits a union pin when
                        // there's only one distinct provider, so
                        // pubgrub can pick whichever provider
                        // version satisfies the wider resolution.
                        selections
                            .entry((interned_virtual, v))
                            .or_default()
                            .push((provider_name.clone(), provider_version.clone()));
                    } else if let Ok(constraint) = Constraint::parse(effective) {
                        wildcards
                            .entry(interned_virtual)
                            .or_default()
                            .push(WildcardProvider {
                                provider_name: provider_name.clone(),
                                provider_version: provider_version.clone(),
                                provided_range: to_range(&constraint),
                            });
                    }
                    // If the value parses as neither a Version nor a
                    // Constraint, skip — losing one virtual entry is
                    // safer than aborting the whole load.
                }
            }
        }
    }

    /// Build synthetic `(Version, LockPackage)` candidates for
    /// `name` from the virtual-provider index, filtered by the
    /// effective stability floor. The synthesized `LockPackage`
    /// carries just enough metadata for the install path to ignore
    /// it cleanly (no `dist`, no `require`); `get_dependencies` is
    /// where the real provider link is injected.
    fn synthesize_virtual_candidates(
        &self,
        name: &str,
        floor: Stability,
    ) -> Vec<(Version, LockPackage)> {
        let index = self.virtual_providers.borrow();
        let Some(entries) = index.get(name) else { return Vec::new() };
        let mut seen: std::collections::HashSet<Version> = std::collections::HashSet::new();
        let mut out: Vec<(Version, LockPackage)> = Vec::new();
        for entry in entries {
            if entry.provided_version.stability() < floor {
                continue;
            }
            if !seen.insert(entry.provided_version.clone()) {
                continue;
            }
            out.push((
                entry.provided_version.clone(),
                LockPackage {
                    name: name.to_string(),
                    version: entry.provided_version.normalized.clone(),
                    version_normalized: Some(entry.provided_version.normalized.clone()),
                    dist: None,
                    source: None,
                    require: BTreeMap::default(),
                    require_dev: BTreeMap::default(),
                    package_type: Some("metapackage".into()),
                    autoload: LockAutoload::default(),
                    autoload_dev: serde_json::Value::Null,
                    replace: BTreeMap::default(),
                    provide: BTreeMap::default(),
                    conflict: BTreeMap::default(),
                    bin: Vec::default(),
                    extra: serde_json::Value::Null,
                    time: None,
                },
            ));
        }
        out
    }

    /// Pick a synthetic candidate version for `name` from a
    /// wildcard provider when no real or specific-virtual candidate
    /// satisfied the consumer's `range`.
    ///
    /// For each `WildcardProvider` whose declared range intersects
    /// `range`, the candidate version is taken from the lower bound
    /// of the intersection (the first version the consumer would
    /// accept that the provider also covers). `virtual_selections`
    /// is updated so `get_dependencies` can route the synthetic
    /// pick back to its concrete provider.
    fn synthesize_wildcard_candidate(
        &self,
        name: &PackageName,
        range: &ComposerRange,
    ) -> Option<Version> {
        let wildcards = self.virtual_wildcards.borrow();
        let entries = wildcards.get(name)?;
        if entries.is_empty() {
            return None;
        }
        // Deterministic order: smallest provider name first.
        let mut sorted: Vec<&WildcardProvider> = entries.iter().collect();
        sorted.sort_by(|a, b| a.provider_name.as_str().cmp(b.provider_name.as_str()));

        for entry in sorted {
            let intersection = range.intersection(&entry.provided_range);
            if intersection.is_empty() {
                continue;
            }
            // Take the intersection's lower bound. `Included(v)` is
            // the cleanest case — that exact version is admissible.
            // `Excluded(v)` would need a "next-greater version,"
            // which we don't have a primitive for; skip those rare
            // forms rather than synthesize a possibly-out-of-range
            // candidate.
            let (lower, _) = intersection.bounding_range()?;
            let picked = match lower {
                std::ops::Bound::Included(v) => v.clone(),
                _ => continue,
            };
            self.virtual_selections.borrow_mut().insert(
                (name.clone(), picked.clone()),
                vec![(entry.provider_name.clone(), entry.provider_version.clone())],
            );
            return Some(picked);
        }
        None
    }

    /// Eagerly walk the require closure from the root requires,
    /// loading every reachable package's metadata before pubgrub
    /// runs. Mirrors Composer's `PoolBuilder::buildPool` — we need
    /// every potential virtual provider in the index before any
    /// `choose_version` is called for a virtual name, otherwise the
    /// resolver can't see e.g. that `guzzlehttp/guzzle` provides
    /// `psr/http-client-implementation`.
    ///
    /// Errors loading a single package (network failure, parse
    /// error) propagate. 404s for individual names are absorbed
    /// silently — that's the expected shape for virtual names that
    /// have no real Packagist entry.
    pub fn pre_fetch_closure(&self) -> Result<(), ProviderError> {
        self.pre_fetch_closure_inner(ClosureProgress::new())
    }

    /// Same as [`Self::pre_fetch_closure`] but suppresses the
    /// progress spinner. Used for the prod-only second pass in
    /// `resolve_for_lockfile`: the work is the same shape as the
    /// first pass but every fetch is a cache hit, so an animated
    /// bar would flash on-and-off in a confusing way. The first
    /// pass already gave the user the visibility they needed.
    pub fn pre_fetch_closure_silent(&self) -> Result<(), ProviderError> {
        self.pre_fetch_closure_inner(ClosureProgress::hidden())
    }

    fn pre_fetch_closure_inner(
        &self,
        progress: ClosureProgress,
    ) -> Result<(), ProviderError> {
        let initial: Vec<PackageName> = self
            .root_deps
            .iter()
            .map(|(n, _)| n.clone())
            .filter(|n| !is_platform(n.as_str()))
            .collect();

        // Current-thread runtime with I/O + time enabled is what
        // drives `reqwest::Client`'s async sockets. The fan-out runs
        // 50-ish concurrent in-flight requests on this single
        // executor thread — no `spawn_blocking`, no nested
        // reqwest-internal runtimes, one connection pool shared
        // across all tasks.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .map_err(|e| ProviderError(format!("building tokio runtime for prefetch: {e}")))?;
        let async_client = bougie_fetch::default_async_client()
            .map_err(|e| ProviderError(format!("building async HTTP client: {e:#}")))?;

        // v1 provider tables: load on the runtime *before* the
        // parallel fan-out so each task sees them as a read-only
        // `Arc<HashMap>` snapshot. Composer v1 repos are rare —
        // one-shot per resolve is the natural granularity, and
        // threading the existing `RefCell` through `Send` tasks
        // would just add Mutex overhead.
        for repo in &self.repos {
            if let Some(RepoProtocol::V1(discovery)) = &repo.protocol {
                if self.v1_provider_tables.borrow().contains_key(&repo.url) {
                    continue;
                }
                let table = runtime
                    .block_on(load_v1_provider_table_async(
                        &async_client,
                        &self.paths,
                        repo,
                        discovery,
                    ))
                    .map_err(|e| {
                        ProviderError(format!(
                            "loading v1 provider table for {}: {e:#}",
                            repo.url,
                        ))
                    })?;
                self.v1_provider_tables
                    .borrow_mut()
                    .insert(repo.url.clone(), table);
            }
        }
        let v1_tables: Arc<FxHashMap<String, FxHashMap<String, String>>> =
            Arc::new(self.v1_provider_tables.borrow().clone());

        let outcomes = runtime.block_on(run_prefetch_fanout(
            async_client,
            self.paths.clone(),
            self.repos.clone(),
            v1_tables,
            initial,
            self.minimum_stability,
            Arc::new(self.stability_flags.clone()),
            prefetch_concurrency_limit(),
            progress,
        ))?;

        // Post-process on the main thread so virtual-provider
        // registration is single-threaded (the registry uses RefCells
        // by design — `ResolveProvider` is one-thread-only once the
        // solver starts). Name-sorted for determinism: the original
        // BFS visit order was implementation-defined (LIFO from
        // `Vec::pop`); name-sort is what someone reading the lockfile
        // would expect.
        let mut sorted = outcomes;
        sorted.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        for outcome in sorted {
            self.apply_virtual_contributions(outcome.contributions);
            self.cache
                .borrow_mut()
                .insert(outcome.name, outcome.filtered);
        }
        Ok(())
    }
}

/// Maximum concurrent metadata HTTP requests during the prefetch
/// fan-out. Override with `BOUGIE_CONCURRENT_FETCHES`. Default 50
/// matches uv's `UV_CONCURRENT_DOWNLOADS` — that's the regime where
/// per-request latency stops being the bottleneck on a typical
/// Packagist resolve without flooding the connection pool.
fn prefetch_concurrency_limit() -> usize {
    std::env::var("BOUGIE_CONCURRENT_FETCHES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(50)
}

/// Result of fetching one package's metadata during the prefetch
/// fan-out. `contributions` is the pre-parsed virtual-provider
/// payload (computed from the *pre-filter* version list inside the
/// worker so `provide`/`replace` clauses on stability-filtered-out
/// versions are still indexed, matching the original sync semantics).
/// `filtered` is what lands in `ResolveProvider::cache`.
struct PrefetchOutcome {
    name: PackageName,
    contributions: VirtualContributions,
    filtered: Vec<(Version, LockPackage)>,
}

/// Drive a Semaphore-bounded BFS over the require closure, fetching
/// each visited package's metadata via `spawn_blocking` so they run
/// concurrently. As each completes, its (filtered) versions' require
/// keys become new BFS nodes. Returns one [`PrefetchOutcome`] per
/// visited name; the caller registers virtuals and populates the
/// resolver's cache from these in deterministic order.
async fn run_prefetch_fanout(
    client: reqwest::Client,
    paths: Paths,
    repos: Vec<Repo>,
    v1_tables: Arc<FxHashMap<String, FxHashMap<String, String>>>,
    initial: Vec<PackageName>,
    minimum_stability: Stability,
    stability_flags: Arc<HashMap<String, Stability>>,
    concurrency: usize,
    progress: ClosureProgress,
) -> Result<Vec<PrefetchOutcome>, ProviderError> {
    use std::collections::HashSet;
    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    // BFS `visited` set keyed by `PackageName` — cheap-clone Arc<str>,
    // so the once-per-name insert + the spawn-task clone are refcount
    // bumps rather than allocations.
    let mut visited: HashSet<PackageName> = HashSet::new();
    let mut tasks: tokio::task::JoinSet<Result<PrefetchOutcome, ProviderError>> =
        tokio::task::JoinSet::new();
    let mut outcomes: Vec<PrefetchOutcome> = Vec::new();

    let spawn_fetch =
        |name: PackageName,
         tasks: &mut tokio::task::JoinSet<Result<PrefetchOutcome, ProviderError>>| {
            let client = client.clone();
            let paths = paths.clone();
            let repos = repos.clone();
            let v1_tables = Arc::clone(&v1_tables);
            let stability_flags = Arc::clone(&stability_flags);
            let sem = Arc::clone(&sem);
            tasks.spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .map_err(|e| ProviderError(format!("semaphore closed: {e}")))?;
                let floor = stability_flags
                    .get(name.as_str())
                    .copied()
                    .unwrap_or(minimum_stability);
                load_real_candidates_isolated(
                    &client,
                    &paths,
                    &repos,
                    &v1_tables,
                    name.as_str(),
                    floor,
                )
                .await
            });
        };

    for name in initial {
        if visited.insert(name.clone()) {
            spawn_fetch(name, &mut tasks);
        }
    }

    while let Some(joined) = tasks.join_next().await {
        let outcome = joined
            .map_err(|e| ProviderError(format!("prefetch task join error: {e}")))??;
        // Tick on completion (not on spawn) so the count reflects
        // packages whose metadata has actually landed. Displayed
        // name flickers across in-flight tasks; users only see the
        // running total most of the time anyway.
        progress.tick(outcome.name.as_str());
        for (_, pkg) in &outcome.filtered {
            for req in pkg.require.keys() {
                if is_platform(req) {
                    continue;
                }
                // Intern transitive require names once at the BFS
                // boundary. The `PackageName` clone going into the
                // visited set + the spawn is a refcount bump.
                let interned = PackageName::from(req.as_str());
                if !visited.insert(interned.clone()) {
                    continue;
                }
                spawn_fetch(interned, &mut tasks);
            }
        }
        outcomes.push(outcome);
    }
    progress.finish();
    Ok(outcomes)
}

/// Stateless counterpart to [`ResolveProvider::load_real_candidates`]
/// — no `&self`, no `RefCell`s, safe to call from a spawned async
/// task. Walks the repo list in priority order, returns the first
/// non-404 hit's stable (+ optionally dev) versions, filters by
/// stability floor, and sorts version-descending. v1 provider tables
/// are passed in pre-loaded so this path never mutates them.
///
/// `await`s the per-repo fetches sequentially within one task —
/// the parallelism is *across* tasks (one per package name) in
/// [`run_prefetch_fanout`].
async fn load_real_candidates_isolated(
    client: &reqwest::Client,
    paths: &Paths,
    repos: &[Repo],
    v1_tables: &FxHashMap<String, FxHashMap<String, String>>,
    name: &str,
    floor: Stability,
) -> Result<PrefetchOutcome, ProviderError> {
    let mut versions: Vec<LockPackage> = Vec::new();
    for repo in repos {
        let stable_md =
            fetch_one_isolated(client, paths, repo, v1_tables, name, Variant::Stable)
                .await
                .map_err(|e| {
                    ProviderError(format!(
                        "fetching metadata for {name} from {}: {e:#}",
                        repo.url,
                    ))
                })?;
        // Union across every repo's hit rather than stopping at the
        // first non-404 — matches the synchronous `load_real_candidates`
        // path (see commit 126d0c6 / PR #135). Magento-style setups
        // commonly have a broad mirror repo listing stale slices
        // alongside a specific repo serving the requested version
        // range; a first-wins filter here would shadow the specific
        // repo and break the resolve.
        let Some(md) = stable_md else { continue };
        if let Some(entries) = md.packages.get(name) {
            versions.extend(entries.iter().cloned());
        }
        if floor == Stability::Dev {
            let dev_md =
                fetch_one_isolated(client, paths, repo, v1_tables, name, Variant::Dev)
                    .await
                    .map_err(|e| {
                        ProviderError(format!(
                            "fetching dev metadata for {name} from {}: {e:#}",
                            repo.url,
                        ))
                    })?;
            if let Some(md) = dev_md
                && let Some(extra) = md.packages.get(name) {
                    versions.extend(extra.iter().cloned());
                }
        }
    }
    // Pre-parse provide/replace clauses while we're already off the
    // main thread. The previous code cloned the entire pre-filter
    // `versions` vec into `raw_versions` so the main thread could
    // call `register_virtuals_from` on it later; doing the parsing
    // here instead removes that clone (~150 ms on a magento2-sized
    // resolve) AND shifts ~150 ms of `Version::parse` /
    // `Constraint::parse` cost off the post-prefetch single-threaded
    // phase.
    // Intern once on the worker; the resulting `PackageName` lives in
    // every contribution + the `PrefetchOutcome::name` field.
    let interned = PackageName::from(name);
    let contributions = compute_virtual_contributions(&interned, &versions);
    let mut filtered: Vec<(Version, LockPackage)> = versions
        .into_iter()
        .filter_map(|p| {
            Version::parse(&p.version)
                .ok()
                .filter(|v| v.stability() >= floor)
                .map(|v| (v, p))
        })
        .collect();
    filtered.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(PrefetchOutcome { name: interned, contributions, filtered })
}

/// Stateless v1/v2 dispatch — same shape as
/// [`ResolveProvider::fetch_one`] but reads pre-loaded v1 provider
/// tables from a passed-in map instead of `RefCell`s on `&self`,
/// so it works inside a spawned async task.
async fn fetch_one_isolated(
    client: &reqwest::Client,
    paths: &Paths,
    repo: &Repo,
    v1_tables: &FxHashMap<String, FxHashMap<String, String>>,
    package: &str,
    variant: Variant,
) -> eyre::Result<Option<bougie_composer::metadata::PackageMetadata>> {
    match &repo.protocol {
        Some(RepoProtocol::V1(discovery)) => {
            if variant == Variant::Dev {
                // v1 stuffs branches into the single per-package
                // document; `Variant::Stable` already returned
                // everything. Skip the redundant request.
                return Ok(None);
            }
            let table = v1_tables.get(&repo.url).ok_or_else(|| {
                eyre::eyre!(
                    "internal: v1 provider table not pre-loaded for {}",
                    repo.url,
                )
            })?;
            fetch_package_metadata_v1_optional_async(
                client, paths, repo, discovery, table, package,
            )
            .await
        }
        _ => {
            fetch_package_metadata_optional_async(client, paths, repo, package, variant)
                .await
        }
    }
}

/// Spinner that ticks once per *completed* fetch during
/// `pre_fetch_closure`, gated on the global `progress_visible` flag
/// (which the CLI flips off for `--quiet` and `--format json`).
/// Hidden in tests + JSON output by construction — `ProgressBar`
/// with a hidden draw target is a cheap no-op for every `tick` /
/// `finish` call.
///
/// The displayed package name is whichever fetch landed most
/// recently; with concurrent fan-out it flickers across in-flight
/// names, and the running count is the real signal anyway. The
/// closure is invoked twice per `composer update` (once for the
/// full graph, once for prod-only) so this is created twice; the
/// second pass typically blows through cache hits in milliseconds
/// and the bar finish-and-clears before the user notices it.
struct ClosureProgress {
    pb: indicatif::ProgressBar,
}

impl ClosureProgress {
    fn new() -> Self {
        if !bougie_output::output::progress_visible() {
            return Self::hidden();
        }
        let pb = indicatif::ProgressBar::new(0);
        pb.set_draw_target(indicatif::ProgressDrawTarget::stderr_with_hz(15));
        let style = indicatif::ProgressStyle::with_template(
            "  fetching metadata  {spinner:.magenta} {pos} packages  {wide_msg:.dim}",
        )
        .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner());
        pb.set_style(style);
        pb.enable_steady_tick(std::time::Duration::from_millis(120));
        Self { pb }
    }

    /// No-op variant. Used by [`ResolveProvider::pre_fetch_closure_silent`]
    /// for the prod-only second pass, where rendering a fresh
    /// spinner would just be noise.
    fn hidden() -> Self {
        let pb = indicatif::ProgressBar::new(0);
        pb.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        Self { pb }
    }

    fn tick(&self, current_package: &str) {
        self.pb.set_message(current_package.to_owned());
        self.pb.inc(1);
    }

    fn finish(&self) {
        self.pb.finish_and_clear();
    }
}

/// Spinner that ticks once per `versions_for` call during the
/// pubgrub `resolve` phase. Same visibility gating as
/// [`ClosureProgress`] — hidden under `--quiet` and `--format json`.
///
/// Pubgrub doesn't expose progress callbacks, but it routes every
/// candidate enumeration through `versions_for`, so the count of
/// visits is a reasonable "is the solver doing work?" signal.
/// Backtracking shows up as the count climbing without resolution
/// — which still beats the silent stall this replaces.
struct SolveProgress {
    pb: indicatif::ProgressBar,
}

impl SolveProgress {
    fn new() -> Self {
        if !bougie_output::output::progress_visible() {
            return Self::hidden();
        }
        let pb = indicatif::ProgressBar::new(0);
        pb.set_draw_target(indicatif::ProgressDrawTarget::stderr_with_hz(15));
        let style = indicatif::ProgressStyle::with_template(
            "  resolving         {spinner:.cyan} {pos} visits     {wide_msg:.dim}",
        )
        .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner());
        pb.set_style(style);
        pb.enable_steady_tick(std::time::Duration::from_millis(120));
        Self { pb }
    }

    fn hidden() -> Self {
        let pb = indicatif::ProgressBar::new(0);
        pb.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        Self { pb }
    }

    fn tick(&self, current_package: &str) {
        self.pb.set_message(current_package.to_owned());
        self.pb.inc(1);
    }

    fn finish(&self) {
        self.pb.finish_and_clear();
    }
}

/// Walk `require` / `require-dev` from composer.json. Returns the
/// list of `(name, range)` pairs plus the per-package stability
/// flags extracted from any `@<stability>` suffix on the constraint
/// string. Composer accepts these only at the root level, so this
/// is the only place they're parsed.
fn read_root_requires(
    composer_json: &Value,
    no_dev: bool,
) -> Result<
    (
        Vec<(PackageName, ComposerRange)>,
        HashMap<String, Stability>,
        FxHashMap<String, String>,
    ),
    BuildError,
> {
    let mut out: Vec<(PackageName, ComposerRange)> = Vec::new();
    let mut flags: HashMap<String, Stability> = HashMap::new();
    let mut raw_constraints: FxHashMap<String, String> = FxHashMap::default();
    let obj = composer_json
        .as_object()
        .ok_or_else(|| BuildError::Internal("composer.json top-level is not an object".into()))?;

    let keys: &[&str] = if no_dev { &["require"] } else { &["require", "require-dev"] };
    for key in keys {
        let Some(reqs) = obj.get(*key).and_then(Value::as_object) else { continue };
        for (dep_name, raw) in reqs {
            if is_platform(dep_name) {
                continue;
            }
            let raw_constraint = raw.as_str().ok_or_else(|| {
                BuildError::Internal(format!("{key}.{dep_name} is not a string"))
            })?;
            let (cleaned, flag) = split_stability_flag(raw_constraint);
            if let Some(stability) = flag {
                flags.insert(dep_name.clone(), stability);
            } else if bougie_semver::version::is_branch_alias(cleaned)
                || (cleaned.len() >= 4
                    && cleaned.as_bytes()[..4].eq_ignore_ascii_case(b"dev-"))
            {
                // The constraint itself targets a dev branch — either
                // the numeric form (`"pdepend/pdepend": "3.x-dev"`)
                // or the bare form (`"acme/module": "dev-main"`).
                // Composer infers a per-package `dev` stability flag
                // in both cases, so the dev document
                // (`/p2/<name>~dev.json`) gets consulted. Matches
                // Composer's `parseStability` which routes
                // `strpos($v, 'dev-') === 0` and
                // `'-dev' === substr($v, -4)` to `'dev'`.
                flags.insert(dep_name.clone(), Stability::Dev);
            }
            let constraint = Constraint::parse(cleaned).map_err(|e| {
                BuildError::ParseConstraint {
                    dep: dep_name.clone(),
                    constraint: raw_constraint.to_owned(),
                    reason: e.to_string(),
                }
            })?;
            raw_constraints.insert(dep_name.clone(), raw_constraint.to_owned());
            out.push((PackageName::from(dep_name.as_str()), to_range(&constraint)));
        }
    }
    Ok((out, flags, raw_constraints))
}

/// Read `minimum-stability` from composer.json's top-level. Default
/// is `Stability::Stable` (Composer's own default). Unknown values
/// surface as a `BuildError`.
fn read_minimum_stability(composer_json: &Value) -> Result<Stability, BuildError> {
    let obj = composer_json.as_object().ok_or_else(|| {
        BuildError::Internal("composer.json top-level is not an object".into())
    })?;
    let Some(value) = obj.get("minimum-stability") else {
        return Ok(Stability::Stable);
    };
    let s = value.as_str().ok_or_else(|| {
        BuildError::Internal("`minimum-stability` is not a string".into())
    })?;
    Stability::parse(s).ok_or_else(|| {
        BuildError::Internal(format!(
            "`minimum-stability` value {s:?} is not a recognised Composer stability \
             (expected one of: dev, alpha, beta, RC, stable)",
        ))
    })
}

/// Read `prefer-stable` from composer.json's top-level. Composer's
/// default is `false`. Non-boolean values surface as a `BuildError`.
fn read_prefer_stable(composer_json: &Value) -> Result<bool, BuildError> {
    let obj = composer_json.as_object().ok_or_else(|| {
        BuildError::Internal("composer.json top-level is not an object".into())
    })?;
    let Some(value) = obj.get("prefer-stable") else {
        return Ok(false);
    };
    value.as_bool().ok_or_else(|| {
        BuildError::Internal(format!(
            "`prefer-stable` must be a boolean (got {value:?})",
        ))
    })
}

/// Read composer.json's `repositories` array into the resolver's
/// repo list. Accepted forms:
///
/// - `{ "type": "composer", "url": "..." }` — adds a Composer-
///   protocol repo (`/p2/<vendor>/<name>.json`). The most common
///   custom-repository pattern (private Packagist mirror, satis).
/// - `{ "packagist.org": false }` — disables the implicit public
///   Packagist repo that would otherwise be appended at the end.
/// - Other forms (`vcs`, `path`, `package`, `artifact`, missing
///   `type`) are ignored silently for this first slice. VCS and
///   path repositories need different machinery and are tracked as
///   follow-ups.
///
/// True for an array-form entry that disables the implicit public
/// Packagist: `{"packagist.org": false}`. The named/object form
/// uses a different spelling (`"packagist.org"` as a top-level key
/// with value `false`) and is handled inline in [`read_repositories`].
fn entry_is_disable_packagist(entry: &serde_json::Map<String, Value>) -> bool {
    entry.get("packagist.org").and_then(Value::as_bool) == Some(false)
}

/// Parse one repository entry (a Composer-protocol `{type, url}`
/// object, or one of the not-yet-supported VCS/path/package/artifact
/// shapes) and append to `repos` on success. Auth is looked up by
/// the URL's host. Used by both wire shapes [`read_repositories`]
/// accepts.
fn parse_repo_entry(
    entry: &serde_json::Map<String, Value>,
    auth: &HashMap<String, crate::metadata::AuthCredentials>,
    repos: &mut Vec<Repo>,
) -> Result<(), BuildError> {
    let r#type = entry.get("type").and_then(Value::as_str);
    match r#type {
        Some("composer") => {
            let url = entry
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    BuildError::Internal(
                        "repository entry of type \"composer\" is missing `url`".into(),
                    )
                })?;
            let repo = Repo::from_url(url);
            let creds = auth.get(&repo.cache_namespace).cloned();
            repos.push(repo.with_auth(creds));
            Ok(())
        }
        Some(
            "vcs" | "github" | "git" | "bitbucket" | "gitlab" | "git-bitbucket" | "hg"
            | "fossil" | "svn",
        ) => {
            // VCS family — not supported yet. Silently ignore so the
            // resolver can still make progress on packages that
            // happen to be on Packagist too. Follow-up issue.
            Ok(())
        }
        Some("path" | "package" | "artifact") => {
            // Same story; non-Composer-protocol shapes need distinct
            // machinery.
            Ok(())
        }
        Some(other) => Err(BuildError::Internal(format!(
            "unknown repository type {other:?}; supported: composer (others coming)",
        ))),
        None => Err(BuildError::Internal(
            "repository entry has no `type` field and is not `packagist.org: false`".into(),
        )),
    }
}

/// Composer's repository ordering: declarations come first (highest
/// priority), with the implicit public Packagist appended last
/// unless disabled. Mirrors `Composer\Repository\
/// RepositoryFactory::createDefaultRepositories`.
///
/// Both wire shapes Composer accepts are honored:
///
/// - Array form: `"repositories": [{"type": "composer", "url": "..."},
///   {"packagist.org": false}]`.
/// - Named/object form: `"repositories": {"foo": {"type": "composer",
///   "url": "..."}, "packagist.org": false}` — the keys name each
///   entry (used as a hint when nothing else identifies it; the
///   per-host cache namespace still comes from the URL itself), and
///   the literal value `false` for `"packagist.org"` disables the
///   implicit public Packagist.
fn read_repositories(
    composer_json: &Value,
    default_packagist: Repo,
    auth: &HashMap<String, crate::metadata::AuthCredentials>,
) -> Result<Vec<Repo>, BuildError> {
    let obj = composer_json.as_object().ok_or_else(|| {
        BuildError::Internal("composer.json top-level is not an object".into())
    })?;
    let mut repos: Vec<Repo> = Vec::new();
    let mut keep_default_packagist = true;
    if let Some(entry) = obj.get("repositories") {
        match entry {
            Value::Array(entries) => {
                for entry in entries {
                    let Some(entry_obj) = entry.as_object() else { continue };
                    if entry_is_disable_packagist(entry_obj) {
                        keep_default_packagist = false;
                        continue;
                    }
                    parse_repo_entry(entry_obj, auth, &mut repos)?;
                }
            }
            Value::Object(named) => {
                for (_name, value) in named {
                    // `"packagist.org": false` — the named-form
                    // disable spelling. Composer also accepts
                    // `false` against any other repo key as "disable
                    // a previously-declared default repo," but
                    // Packagist is the only default we ship, so we
                    // only handle the one well-defined case.
                    if value.as_bool() == Some(false) {
                        if _name == "packagist.org" {
                            keep_default_packagist = false;
                        }
                        continue;
                    }
                    let Some(entry_obj) = value.as_object() else { continue };
                    parse_repo_entry(entry_obj, auth, &mut repos)?;
                }
            }
            _ => {
                return Err(BuildError::Internal(
                    "`repositories` must be an array or an object".into(),
                ));
            }
        }
    }
    if keep_default_packagist {
        // Public Packagist gets auth attached too if the user has
        // credentials for repo.packagist.org (rare but valid for
        // private-packagist.com / similar hosted mirrors that share
        // the same host).
        let creds = auth.get(&default_packagist.cache_namespace).cloned();
        repos.push(default_packagist.with_auth(creds));
    }
    if repos.is_empty() {
        // A project that explicitly disabled Packagist but declared
        // no replacement repos: nothing to resolve against.
        return Err(BuildError::Internal(
            "no repositories configured: `packagist.org: false` set with no replacement repo"
                .into(),
        ));
    }
    Ok(repos)
}

/// Parse an `{http-basic, bearer}` auth object — the shape that lives
/// at the top level of `auth.json` and (nested under `config`) inside
/// `composer.json`. Error messages prefix each path with `source` so
/// diagnostics name the file/env-var the bad value came from.
///
/// Out of scope: `github-oauth`, `gitlab-token`, `gitlab-oauth`,
/// `bitbucket-oauth`, `client-certificate`, `forgejo-token`,
/// `custom-headers` — Composer reads these from the same containers,
/// but bougie only fetches from URLs that need basic / bearer creds
/// today. The follow-up that wires github-oauth will extend this
/// parser without changing the outer reader functions.
fn parse_auth_object(
    obj: &serde_json::Map<String, Value>,
    source: &str,
) -> Result<HashMap<String, crate::metadata::AuthCredentials>, BuildError> {
    use crate::metadata::AuthCredentials;
    let mut out: HashMap<String, AuthCredentials> = HashMap::new();
    if let Some(bearer) = obj.get("bearer").and_then(Value::as_object) {
        for (host, val) in bearer {
            let token = val.as_str().ok_or_else(|| {
                BuildError::Internal(format!(
                    "{source}: bearer.{host} must be a string token",
                ))
            })?;
            out.insert(host.clone(), AuthCredentials::Bearer { token: token.to_owned() });
        }
    }
    if let Some(github) = obj.get("github-oauth").and_then(Value::as_object) {
        for (host, val) in github {
            let token = val.as_str().ok_or_else(|| {
                BuildError::Internal(format!(
                    "{source}: github-oauth.{host} must be a string token",
                ))
            })?;
            out.insert(host.clone(), AuthCredentials::GitHubToken { token: token.to_owned() });
        }
    }
    if let Some(gitlab) = obj.get("gitlab-oauth").and_then(Value::as_object) {
        for (host, val) in gitlab {
            let token = val.as_str().ok_or_else(|| {
                BuildError::Internal(format!(
                    "{source}: gitlab-oauth.{host} must be a string token",
                ))
            })?;
            out.insert(host.clone(), AuthCredentials::Bearer { token: token.to_owned() });
        }
    }
    if let Some(gitlab) = obj.get("gitlab-token").and_then(Value::as_object) {
        for (host, val) in gitlab {
            let token = if let Some(s) = val.as_str() {
                s.to_owned()
            } else if let Some(obj) = val.as_object() {
                obj.get("token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        BuildError::Internal(format!(
                            "{source}: gitlab-token.{host}.token is missing or not a string",
                        ))
                    })?
                    .to_owned()
            } else {
                return Err(BuildError::Internal(format!(
                    "{source}: gitlab-token.{host} must be a string or object with `token`",
                )));
            };
            out.insert(host.clone(), AuthCredentials::GitLabToken { token });
        }
    }
    if let Some(http_basic) = obj.get("http-basic").and_then(Value::as_object) {
        for (host, val) in http_basic {
            let entry = val.as_object().ok_or_else(|| {
                BuildError::Internal(format!(
                    "{source}: http-basic.{host} must be an object with `username` and `password`",
                ))
            })?;
            let username = entry
                .get("username")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    BuildError::Internal(format!(
                        "{source}: http-basic.{host}.username is missing or not a string",
                    ))
                })?;
            let password = entry
                .get("password")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    BuildError::Internal(format!(
                        "{source}: http-basic.{host}.password is missing or not a string",
                    ))
                })?;
            out.insert(
                host.clone(),
                AuthCredentials::Basic {
                    username: username.to_owned(),
                    password: password.to_owned(),
                },
            );
        }
    }
    Ok(out)
}

/// Read auth credentials from composer.json's `config.http-basic`
/// and `config.bearer` maps. This is the lowest-precedence source —
/// see [`read_all_auth`] for the full merge order.
pub fn read_auth_from_composer_json(
    composer_json: &Value,
) -> Result<HashMap<String, crate::metadata::AuthCredentials>, BuildError> {
    let Some(obj) = composer_json.as_object() else {
        return Ok(HashMap::new());
    };
    let Some(config) = obj.get("config").and_then(Value::as_object) else {
        return Ok(HashMap::new());
    };
    parse_auth_object(config, "composer.json config")
}

/// Read auth from a project-level `auth.json` (next to
/// composer.json). The shape is the same as composer.json's
/// `config` section but at the top level. Returns an empty map if
/// the file doesn't exist.
pub fn read_auth_json(
    project_root: &Path,
) -> Result<HashMap<String, crate::metadata::AuthCredentials>, BuildError> {
    read_auth_json_at(&project_root.join("auth.json"))
}

/// Read auth from a specific `auth.json` path. Returns an empty map
/// when the file doesn't exist; otherwise parses with [`parse_auth_object`].
/// Split out from [`read_auth_json`] so the global-auth path picker
/// can reuse it without restating the read-then-parse dance.
pub(crate) fn read_auth_json_at(
    path: &Path,
) -> Result<HashMap<String, crate::metadata::AuthCredentials>, BuildError> {
    if !path.is_file() {
        return Ok(HashMap::new());
    }
    let bytes = std::fs::read(path).map_err(|e| {
        BuildError::Internal(format!("reading {}: {e}", path.display()))
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
        BuildError::Internal(format!("parsing {}: {e}", path.display()))
    })?;
    let Some(obj) = value.as_object() else {
        return Ok(HashMap::new());
    };
    parse_auth_object(obj, &path.display().to_string())
}

/// Candidate locations bougie probes for the **global** Composer
/// `auth.json`, in lookup order. Composer itself reads from
/// `$COMPOSER_HOME/auth.json` (see `Composer\Factory::createConfig`,
/// `Factory.php:209`); we add the XDG-strict and legacy locations
/// because that's where the file actually lives across distributions.
///
/// First existing file in the returned list wins.
///
/// Taking `env` as a closure keeps the function pure for unit tests
/// (no `std::env::set_var` races between threads). The public
/// [`read_global_auth_json`] wires it to `std::env::var`.
pub(crate) fn global_auth_json_candidates(env: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = env("COMPOSER_HOME") {
        out.push(PathBuf::from(h).join("auth.json"));
    }
    if let Some(h) = env("XDG_CONFIG_HOME") {
        out.push(PathBuf::from(h).join("composer").join("auth.json"));
    }
    if let Some(h) = env("HOME") {
        // XDG default and the historical Composer location, in
        // Composer's own preference order.
        out.push(PathBuf::from(&h).join(".config").join("composer").join("auth.json"));
        out.push(PathBuf::from(&h).join(".composer").join("auth.json"));
    }
    out
}

/// Read auth from the global Composer `auth.json`, if present.
/// Returns an empty map when no candidate file exists. See
/// [`global_auth_json_candidates`] for the lookup order.
///
/// The high-value point of this source: agencies already keep their
/// GitHub / private-Packagist credentials in
/// `~/.config/composer/auth.json` for Composer itself. Reading the
/// same file for free means bougie inherits a working credential
/// store without any reconfiguration.
pub fn read_global_auth_json() -> Result<HashMap<String, crate::metadata::AuthCredentials>, BuildError>
{
    for candidate in global_auth_json_candidates(|k| std::env::var(k).ok()) {
        if candidate.is_file() {
            return read_auth_json_at(&candidate);
        }
    }
    Ok(HashMap::new())
}

/// Parse the JSON body of the `COMPOSER_AUTH` environment variable.
/// The shape is the same as `auth.json` — a top-level object with
/// `http-basic` / `bearer` (and Composer's other auth keys, which
/// bougie skips today). Empty input → empty map.
pub fn parse_composer_auth_env(
    raw: &str,
) -> Result<HashMap<String, crate::metadata::AuthCredentials>, BuildError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(HashMap::new());
    }
    let value: Value = serde_json::from_str(trimmed).map_err(|e| {
        BuildError::Internal(format!("COMPOSER_AUTH is not valid JSON: {e}"))
    })?;
    let Some(obj) = value.as_object() else {
        return Err(BuildError::Internal(
            "COMPOSER_AUTH must decode to a JSON object".into(),
        ));
    };
    parse_auth_object(obj, "COMPOSER_AUTH")
}

/// Read auth from the `COMPOSER_AUTH` environment variable. The
/// canonical way to inject Composer credentials in CI without
/// committing an `auth.json` to the repo. Returns an empty map when
/// the variable is unset or empty.
pub fn read_composer_auth_env() -> Result<HashMap<String, crate::metadata::AuthCredentials>, BuildError>
{
    match std::env::var("COMPOSER_AUTH") {
        Ok(s) => parse_composer_auth_env(&s),
        Err(_) => Ok(HashMap::new()),
    }
}

/// Collect auth credentials from every source bougie understands and
/// merge them in Composer's documented order. Later inserts win, so
/// the order below — global `auth.json`, composer.json `config`,
/// project `auth.json`, `COMPOSER_AUTH` (highest) — is the effective
/// priority.
///
/// This is what Composer itself does (see `Composer\Factory.php`
/// lines 209-219 for the global + COMPOSER_AUTH-first-pass, then
/// 328-344 for composer.json + project auth.json + COMPOSER_AUTH
/// second pass). Composer applies `COMPOSER_AUTH` twice — once after
/// global auth.json, once again at the very end — explicitly so it
/// wins over both composer.json `config` and the project `auth.json`
/// ("make sure we load the auth env again over the local auth.json +
/// composer.json config"). We collapse that into one final apply
/// since the first pass is dominated by everything that follows.
///
/// Intuition: global is the user's machine defaults; composer.json
/// `config` is the project's committed intent; project `auth.json` is
/// the developer's per-checkout override; `COMPOSER_AUTH` is the
/// CI/runtime override.
pub fn read_all_auth(
    composer_json: &Value,
    project_root: &Path,
) -> Result<HashMap<String, crate::metadata::AuthCredentials>, BuildError> {
    Ok(merge_auth_sources(
        read_global_auth_json()?,
        read_auth_from_composer_json(composer_json)?,
        read_auth_json(project_root)?,
        read_composer_auth_env()?,
    ))
}

/// Pure merger so the precedence order can be unit-tested without
/// env-var or filesystem races. Arguments are listed lowest- to
/// highest-precedence; the returned map carries the per-host winner.
pub(crate) fn merge_auth_sources(
    global: HashMap<String, crate::metadata::AuthCredentials>,
    composer_json_config: HashMap<String, crate::metadata::AuthCredentials>,
    project_auth_json: HashMap<String, crate::metadata::AuthCredentials>,
    composer_auth_env: HashMap<String, crate::metadata::AuthCredentials>,
) -> HashMap<String, crate::metadata::AuthCredentials> {
    let mut out = global;
    out.extend(composer_json_config);
    out.extend(project_auth_json);
    out.extend(composer_auth_env);
    out
}

/// Format a version list for error messages. Short lists are shown in
/// full; longer lists are abbreviated with `..` like Composer does:
/// `1.0.0, 2.10.0, .., 2.14.0`.
fn format_version_list(versions: impl IntoIterator<Item = String>) -> String {
    let vs: Vec<String> = versions.into_iter().collect();
    if vs.len() <= 4 {
        vs.join(", ")
    } else {
        format!("{}, {}, .., {}", vs[0], vs[1], vs[vs.len() - 1])
    }
}

/// Split a Composer constraint string into its constraint body and
/// trailing `@<stability>` flag. Returns `(body, Some(stability))` or
/// `(input, None)` when no flag is present. Composer's grammar puts
/// the flag at the end after a literal `@`, e.g. `"^1.0@dev"`.
fn split_stability_flag(raw: &str) -> (&str, Option<Stability>) {
    if let Some(idx) = raw.rfind('@') {
        let suffix = &raw[idx + 1..];
        if let Some(stab) = Stability::parse(suffix) {
            return (raw[..idx].trim_end(), Some(stab));
        }
    }
    (raw, None)
}

#[derive(Debug, Clone)]
pub enum BuildError {
    ParseConstraint {
        dep: String,
        constraint: String,
        reason: String,
    },
    Internal(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseConstraint { dep, constraint, reason } => write!(
                f,
                "constraint {constraint:?} on `{dep}` is invalid: {reason}",
            ),
            Self::Internal(s) => write!(f, "internal error: {s}"),
        }
    }
}

impl std::error::Error for BuildError {}

/// Parse each `(dep_name, constraint_str)` pair from one of the
/// `require` / `replace` / `provide` maps and append it to `out` as
/// a pubgrub constraint. Platform packages are skipped (their
/// version check is tracked separately in issue #118). `clause_kind`
/// is the source label used in error messages so a malformed
/// constraint reports whether it came from a require, a replace,
/// or a provide.
///
/// `owner_version` is the version string of the package the map
/// belongs to. Used to resolve Composer's `"self.version"` sentinel
/// — common in `replace` declarations like
/// `ramsey/uuid 4.9.2 replace: { rhumsaa/uuid: "self.version" }`,
/// meaning "I replace rhumsaa/uuid at exactly my own version."
fn push_constraint_map(
    out: &mut Vec<(PackageName, ComposerRange)>,
    map: &std::collections::BTreeMap<String, String>,
    owner: &str,
    owner_version: &str,
    clause_kind: &'static str,
) -> Result<(), ProviderError> {
    for (dep_name, raw) in map {
        if is_platform(dep_name) {
            continue;
        }
        let effective = if raw == "self.version" {
            owner_version
        } else {
            raw
        };
        // Strip any trailing `@<stability>` flag — Composer accepts
        // these on transitive requires too (e.g.
        // `"codeception/codeception": "*@dev"`) but doesn't use them
        // for matching at the transitive level. For our purposes,
        // ignore the flag and parse the remainder.
        let (cleaned, _flag) = split_stability_flag(effective);
        let constraint = Constraint::parse(cleaned).map_err(|e| {
            ProviderError(format!(
                "constraint {raw:?} on `{dep_name}` ({clause_kind} from `{owner}`): {e}",
            ))
        })?;
        out.push((PackageName::from(dep_name.as_str()), to_range(&constraint)));
    }
    Ok(())
}

/// Conflict declarations: `{ "dep": "<7.4" }` means "cannot coexist
/// with dep at versions matching `<7.4`". Expressed as a pubgrub
/// dependency on the *complement* range — "requires dep NOT `<7.4`"
/// = "requires dep `>=7.4`". Caller pre-filters to only include
/// entries whose target is already in the dependency graph.
fn push_conflict_map(
    out: &mut Vec<(PackageName, ComposerRange)>,
    map: &std::collections::BTreeMap<String, String>,
    owner: &str,
    owner_version: &str,
) -> Result<(), ProviderError> {
    for (dep_name, raw) in map {
        if is_platform(dep_name) {
            continue;
        }
        let effective = if raw == "self.version" {
            owner_version
        } else {
            raw
        };
        let (cleaned, _flag) = split_stability_flag(effective);
        let constraint = Constraint::parse(cleaned).map_err(|e| {
            ProviderError(format!(
                "conflict constraint {raw:?} on `{dep_name}` (conflict from `{owner}`): {e}",
            ))
        })?;
        let conflict_range = to_range(&constraint);
        let allowed = conflict_range.complement();
        if allowed != ComposerRange::empty() {
            out.push((PackageName::from(dep_name.as_str()), allowed));
        }
    }
    Ok(())
}

impl DependencyProvider for ResolveProvider {
    type P = PubGrubPackage;
    type V = Version;
    type VS = ComposerRange;
    // `Reverse` flips the natural ordering on `u32` so the lowest
    // candidate count maps to the highest priority. pubgrub picks
    // the package with the largest priority on every iteration; we
    // want it to pick whichever package has the fewest in-range
    // candidates first (Tsai's "fewer candidates first"), so the
    // resolver doesn't waste exploration on flexible packages while
    // a tight constraint is sitting unsolved waiting to fail.
    type Priority = std::cmp::Reverse<u32>;
    type M = String;
    type Err = ProviderError;

    fn prioritize(
        &self,
        package: &Self::P,
        range: &Self::VS,
        _stats: &PackageResolutionStatistics,
    ) -> Self::Priority {
        // Root is always decided first. `Reverse(0)` is the highest
        // value the `Reverse<u32>` ordering admits.
        let PubGrubPackage::Package(name) = package else {
            return std::cmp::Reverse(0);
        };
        // Peek the already-populated caches; never fetch from
        // `prioritize`. pubgrub calls this constantly (every time the
        // partial solution changes), and pre-fetch has already closed
        // by the time the solve loop runs, so a real candidate list
        // either lives in `merged_cache` (post-`versions_for` shape)
        // or `cache` (raw pre-fetch staging) by now. For virtual
        // names, we additionally consult `virtual_providers` — those
        // entries never land in `cache` because they're synthesized
        // lazily by `synthesize_virtual_candidates`.
        let count = self.priority_count(name.as_str(), range);
        // Packages we don't have a cache entry for yet (rare —
        // typically a virtual whose provider isn't loaded) sort
        // between the well-known few-candidate names and the broad
        // many-candidate names. Avoid `u32::MAX` (would never get
        // picked) and `0` (would short-circuit Root); the midpoint
        // is a safe "unknown so go ahead of broad packages, behind
        // narrow ones" guess until the cache populates and a later
        // `prioritize` call refines it.
        std::cmp::Reverse(count.unwrap_or(u32::MAX / 2))
    }

    fn choose_version(
        &self,
        package: &Self::P,
        range: &Self::VS,
    ) -> Result<Option<Self::V>, Self::Err> {
        match package {
            PubGrubPackage::Root => Ok(if range.contains(&self.root_version) {
                Some(self.root_version.clone())
            } else {
                None
            }),
            PubGrubPackage::Package(name) => {
                let versions = self.versions_for(name.as_str())?;
                // `versions_for` sorts descending. With
                // `prefer-stable`, do a two-pass scan: first pick the
                // highest *stable* in range, then fall back to the
                // highest in range regardless of stability. Without
                // prefer-stable the second pass alone is used.
                //
                // Cheap optimization: when the effective floor is
                // already `Stable`, every candidate is stable, so
                // the prefer-stable pass would just duplicate the
                // fallback. Skip it.
                let excludes = self.conflict_excludes.borrow();
                let is_excluded = |v: &Version| -> bool {
                    !excludes.is_empty() && excludes.contains(&(name.clone(), v.clone()))
                };
                let floor = self.effective_stability(name.as_str());
                if self.prefer_stable && floor != Stability::Stable {
                    for (v, _) in versions.iter() {
                        if v.stability() == Stability::Stable && range.contains(v) && !is_excluded(v) {
                            return Ok(Some(v.clone()));
                        }
                    }
                }
                for (v, _) in versions.iter() {
                    if range.contains(v) && !is_excluded(v) {
                        return Ok(Some(v.clone()));
                    }
                }
                // Wildcard fallback: when a real package declared
                // `replace: { name: "*" }` (or any range-shaped
                // clause), it stands in for *any* version of `name`
                // that fits the consumer's range. Synthesize a
                // candidate at the first version inside the
                // consumer's range that the wildcard provider also
                // covers.
                if let Some(picked) = self.synthesize_wildcard_candidate(name, range) {
                    return Ok(Some(picked));
                }
                Ok(None)
            }
        }
    }

    fn get_dependencies(
        &self,
        package: &Self::P,
        version: &Self::V,
    ) -> Result<Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        // Root is asked exactly once; skip the cache so we don't pay
        // the hash + insert cost or the `Arc` allocation for a path
        // that doesn't repeat.
        if matches!(package, PubGrubPackage::Root) {
            let constraints: DependencyConstraints<Self::P, Self::VS> = self
                .root_deps
                .iter()
                .cloned()
                .map(|(n, r)| (PubGrubPackage::Package(n), r))
                .collect();
            return Ok(Dependencies::Available(constraints));
        }
        let parsed = self.parsed_deps_for(package, version)?;
        // Re-collect into a fresh `DependencyConstraints` per call —
        // pubgrub consumes it by value. The cached slice is shared
        // via `Arc`; only the `(P, VS)` pair clones run on a hit
        // (avoiding the `Constraint::parse` + `to_range` per dep).
        let constraints: DependencyConstraints<Self::P, Self::VS> =
            parsed.iter().cloned().collect();
        Ok(Dependencies::Available(constraints))
    }
}

impl ResolveProvider {
    /// Count the in-range candidates for `name` without triggering a
    /// fetch. Used by `prioritize` to feed the "fewer candidates
    /// first" heuristic. Returns `None` when neither cache holds an
    /// entry for `name` — `prioritize` falls back to a default in
    /// that case rather than blocking on a network round-trip.
    ///
    /// Lookup order matches `versions_for`'s logical order:
    /// post-merge `merged_cache` first (the form `choose_version`
    /// would actually see), then the raw pre-fetch `cache` (with
    /// virtuals merged from `virtual_providers`). The latter
    /// approximates what `versions_for` would synthesize once it's
    /// asked; close enough for an ordering heuristic.
    fn priority_count(&self, name: &str, range: &ComposerRange) -> Option<u32> {
        if let Some(entries) = self.merged_cache.borrow().get(name) {
            let n = entries
                .iter()
                .filter(|(v, _)| range.contains(v))
                .count();
            return Some(u32::try_from(n).unwrap_or(u32::MAX));
        }
        let real_count = self
            .cache
            .borrow()
            .get(name)
            .map(|entries| entries.iter().filter(|(v, _)| range.contains(v)).count())
            .unwrap_or(0);
        let virtual_count = self
            .virtual_providers
            .borrow()
            .get(name)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|e| range.contains(&e.provided_version))
                    .count()
            })
            .unwrap_or(0);
        let total = real_count + virtual_count;
        if total == 0 {
            None
        } else {
            Some(u32::try_from(total).unwrap_or(u32::MAX))
        }
    }

    /// Parsed-deps cache lookup + populate. Returns the shared
    /// `Arc<Vec<_>>` for `(package, version)`; pubgrub's
    /// `get_dependencies` rebuilds a fresh `DependencyConstraints`
    /// from the slice on every call (the per-call clone of two
    /// items — `PubGrubPackage` + `ComposerRange` — is cheap next to
    /// the `Constraint::parse` + `to_range` work the cache skips).
    ///
    /// Caller must ensure `package` is `PubGrubPackage::Package(_)`;
    /// `Root` is handled inline in `get_dependencies` to skip the
    /// hash + insert cost on a path that doesn't repeat.
    fn parsed_deps_for(
        &self,
        package: &PubGrubPackage,
        version: &Version,
    ) -> Result<Arc<Vec<(PubGrubPackage, ComposerRange)>>, ProviderError> {
        let key = (package.clone(), version.clone());
        if let Some(hit) = self.parsed_deps.borrow().get(&key) {
            return Ok(Arc::clone(hit));
        }
        let parsed = self.compute_parsed_deps(package, version)?;
        let arc = Arc::new(parsed);
        self.parsed_deps
            .borrow_mut()
            .insert(key, Arc::clone(&arc));
        Ok(arc)
    }

    /// Cache-miss path: do the actual work `get_dependencies` used
    /// to do inline. Covers both the virtual-selection branch and
    /// the real-package branch, in that order — the same priority
    /// the pre-refactor body had.
    fn compute_parsed_deps(
        &self,
        package: &PubGrubPackage,
        version: &Version,
    ) -> Result<Vec<(PubGrubPackage, ComposerRange)>, ProviderError> {
        let PubGrubPackage::Package(name) = package else {
            // Caller guarantees this. If it ever reaches here, the
            // refactor lost the early return in `get_dependencies` —
            // surface as an internal error rather than panicking.
            return Err(ProviderError(
                "internal: parsed_deps_for called with Root".to_owned(),
            ));
        };

        // Virtual-selection path: if pubgrub picked a synthetic
        // candidate (from `synthesize_virtual_candidates` or
        // `synthesize_wildcard_candidate`), emit a tight pin on the
        // concrete provider instead of real deps.
        //
        // Gate: only use the virtual path when the entry in the
        // merged cache is synthetic (metapackage, no dist) — or when
        // the version isn't in the cache at all (wildcard-synthesized
        // candidates are created in `choose_version`, not in
        // `versions_for`). When `versions_for` deduped a virtual
        // against a real entry at the same version, the cached entry
        // is real and should use the real-package path even though
        // `virtual_selections` still contains a stale registration.
        let versions = self.versions_for(name.as_str())?;
        let cached_entry = versions.iter().find(|(v, _)| v == version);
        let is_real = cached_entry
            .is_some_and(|(_, e)| e.dist.is_some() || e.package_type.as_deref() != Some("metapackage"));

        if !is_real {
            let virtual_key = (name.clone(), version.clone());
            if let Some(providers) =
                self.virtual_selections.borrow().get(&virtual_key).cloned()
            {
                let mut by_name: std::collections::BTreeMap<PackageName, Vec<Version>> =
                    std::collections::BTreeMap::new();
                for (pname, pver) in &providers {
                    by_name.entry(pname.clone()).or_default().push(pver.clone());
                }
                let mut out: Vec<(PubGrubPackage, ComposerRange)> = Vec::new();
                // Pick the provider to pin. With a single distinct
                // provider that's the obvious choice. When several
                // distinct providers compete (the "multiple PSR
                // implementations" case), fall back to the *first
                // registration* — emitting nothing here would leave the
                // virtual with zero concrete providers and then drop it
                // from the lock entirely.
                let chosen = if by_name.len() == 1 {
                    by_name.keys().next().cloned()
                } else {
                    providers.first().map(|(pname, _)| pname.clone())
                };
                if let Some(pname) = chosen {
                    let versions = by_name.get(&pname).expect("chosen provider is in by_name");
                    let range = versions
                        .iter()
                        .map(|v| {
                            to_range(&Constraint::Op {
                                op: bougie_semver::version::CmpOp::Eq,
                                version: v.clone(),
                                explicit_lower_bound: true,
                            })
                        })
                        .fold(ComposerRange::empty(), |acc, r| acc.union(&r));
                    out.push((PubGrubPackage::Package(pname), range));
                }
                return Ok(out);
            }
        }

        let Some((_, entry)) = cached_entry else {
            return Err(ProviderError(format!(
                "internal: get_dependencies({name}@{version}) but version not in cache",
            )));
        };

        let mut staging: Vec<(PackageName, ComposerRange)> = Vec::new();
        let owner_version = &entry.version;
        push_constraint_map(
            &mut staging,
            &entry.require,
            name.as_str(),
            owner_version,
            "require",
        )?;
        // `replace` is encoded as an additional require when the
        // clause is a bare version (`replace: { sub: 2.0.0 }`). This
        // emits `sub: ==2.0.0` from the replacer, enforcing
        // Composer's "no coexistence at wrong version" rule during
        // solving. The post-solve filter then removes replaced
        // packages whose replacer is in the solution (preventing
        // double-installation of e.g. `illuminate/support` alongside
        // `laravel/framework`).
        //
        // Range/wildcard replaces flow through the virtual-provider
        // index alone. `provide` is capability-only; never a require.
        let exact_replaces: std::collections::BTreeMap<String, String> = entry
            .replace
            .iter()
            .filter(|(k, v)| {
                // Only emit the coexistence constraint if the
                // replaced package is actually in the dependency
                // graph. The pre-fetch BFS populates the cache for
                // every reachable package; if the replaced name isn't
                // there, nothing requires it and pulling it in would
                // bloat the graph (e.g. magento/community-edition
                // replaces 238 modules, most unrequired by anything).
                // Also check root requires: pubgrub might call
                // get_dependencies for the replacer before loading
                // the replaced package's metadata.
                let in_graph = self.cache.borrow().contains_key(k.as_str())
                    || self.merged_cache.borrow().contains_key(k.as_str())
                    || self.root_deps.iter().any(|(n, _)| n.as_str() == k.as_str());
                if !in_graph {
                    return false;
                }
                let effective = if v.as_str() == "self.version" {
                    owner_version.as_str()
                } else {
                    v.as_str()
                };
                Version::parse(effective).is_ok()
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        push_constraint_map(
            &mut staging,
            &exact_replaces,
            name.as_str(),
            owner_version,
            "replace",
        )?;
        {
            let conflict_in_graph: std::collections::BTreeMap<String, String> = entry
                .conflict
                .iter()
                .filter(|(k, _)| {
                    entry.require.contains_key(k.as_str())
                        || self.root_deps.iter().any(|(n, _)| n.as_str() == k.as_str())
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            push_conflict_map(
                &mut staging,
                &conflict_in_graph,
                name.as_str(),
                owner_version,
            )?;
        }
        Ok(staging
            .into_iter()
            .map(|(n, r)| (PubGrubPackage::Package(n), r))
            .collect())
    }
}

/// One resolved package in the dry-run output.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
}

/// Result of `dry_run_update`: the package set pubgrub would write
/// to `composer.lock` if we were committing the update. Ordering is
/// stable (sorted by package name) so two consecutive dry runs
/// produce identical output.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateSummary {
    pub packages: Vec<ResolvedPackage>,
    pub no_dev: bool,
}

/// Options for [`dry_run_update`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DryRunOptions {
    pub no_dev: bool,
}

/// Run a pubgrub-backed resolve of `composer.json` against the
/// metadata host(s) and return the package set that would land in
/// `composer.lock`.
///
/// Read-only: doesn't write `composer.lock`, doesn't touch `vendor/`.
///
/// `default_packagist` is the implicit public Packagist repo
/// (production callers pass [`crate::metadata::Repo::packagist`];
/// tests pass a `Repo::from_url(mock_uri)`). composer.json's
/// `repositories` field augments / overrides this list.
pub fn dry_run_update(
    paths: &Paths,
    project_root: &Path,
    default_packagist: Repo,
    opts: DryRunOptions,
) -> Result<UpdateSummary> {
    let composer_json_path = project_root.join("composer.json");
    if !composer_json_path.is_file() {
        return Err(eyre!(
            "{} not found — not a Composer project",
            composer_json_path.display(),
        ));
    }
    let composer_json_bytes = std::fs::read(&composer_json_path)
        .wrap_err_with(|| format!("reading {}", composer_json_path.display()))?;
    let composer_json: Value = serde_json::from_slice(&composer_json_bytes)
        .map_err(|e| eyre!("parsing composer.json: {e}"))?;

    let client = build_client()?;
    // Auth assembly: composer.json `config`, global Composer
    // `auth.json`, project `auth.json`, then `COMPOSER_AUTH`. See
    // [`read_all_auth`] for the precedence rationale.
    let auth = read_all_auth(&composer_json, project_root).map_err(|e| eyre!(e))?;
    let mut provider = ResolveProvider::build_with_auth(
        client,
        paths.clone(),
        default_packagist,
        &composer_json,
        opts.no_dev,
        auth,
    )
    .map_err(|e| eyre!(e))?;
    // Probe each repo's `packages.json` and record the discovered
    // Composer protocol (v1 vs v2) so the per-package fetcher
    // dispatches correctly. Required before any pre-fetch traffic.
    let t_discover = std::time::Instant::now();
    provider.discover_repos();
    tracing::info!(
        elapsed_ms = crate::elapsed_ms(t_discover.elapsed()),
        repos = provider.repos.len(),
        "discover_repos",
    );
    let t_prefetch = std::time::Instant::now();
    provider
        .pre_fetch_closure()
        .map_err(|e| eyre!("pre-fetching metadata closure: {}", e.0))?;
    tracing::info!(
        elapsed_ms = crate::elapsed_ms(t_prefetch.elapsed()),
        cached_packages = provider.cache_size(),
        "pre_fetch_closure",
    );
    let root = provider.root_version();

    provider.begin_solve_progress();
    let t_solve = std::time::Instant::now();
    // Solve loop: after each solve, validate conflict declarations.
    // If a package in the solution declares a conflict violated by
    // another package in the solution, exclude the violating version
    // and re-solve. Bounded to avoid infinite loops.
    let mut result = resolve(&provider, PubGrubPackage::Root, root.clone());
    for _retry in 0..10 {
        let Ok(ref solution) = result else { break };
        let pairs: Vec<_> = solution.iter().collect();
        let violations = provider.check_conflict_violations(&pairs);
        if violations.is_empty() {
            break;
        }
        tracing::debug!(
            count = violations.len(),
            excluded = ?violations,
            "conflict violations found, re-solving",
        );
        {
            let mut excludes = provider.conflict_excludes.borrow_mut();
            for (name, ver) in violations {
                excludes.insert((name, ver));
            }
        }
        provider.reset_for_re_solve();
        result = resolve(&provider, PubGrubPackage::Root, root.clone());
    }
    let solve_elapsed = t_solve.elapsed();
    provider.finish_solve_progress();
    tracing::info!(
        elapsed_ms = crate::elapsed_ms(solve_elapsed),
        ok = result.is_ok(),
        "pubgrub_resolve",
    );
    // Retry exhaustion guard: the loop above can exit after 10 attempts
    // with a solution that *still* violates a conflict (e.g. the
    // conflict-declaring package has only one version, so excluding it
    // makes no progress). Returning that solution would write a lock
    // that violates a declared conflict — error instead.
    if let Ok(ref solution) = result {
        let pairs: Vec<_> = solution.iter().collect();
        let remaining = provider.check_conflict_violations(&pairs);
        if !remaining.is_empty() {
            let detail = remaining
                .iter()
                .map(|(name, ver)| format!("{name}@{ver}"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(eyre!(
                "could not satisfy declared package conflicts after 10 resolution \
                 attempts; still violated by: {detail}"
            ));
        }
    }
    let summary = match result {
        Ok(solution) => {
            let virtual_selections = provider.virtual_selections.borrow();
            // Build a set of package names that are replaced by
            // another package in the solution. When a replacer (e.g.
            // `laravel/framework`) is in the solution and its replace
            // clause pulled the replaced package in during solving,
            // the replaced package should not appear in the final
            // output — the replacer provides it.
            let solution_pairs: Vec<_> = solution.iter().collect();
            let replaced_names = collect_active_replaces(&provider, &solution_pairs);
            let mut packages: Vec<ResolvedPackage> = solution
                .into_iter()
                .filter_map(|(pkg, version)| match pkg {
                    PubGrubPackage::Root => None,
                    PubGrubPackage::Package(name) => {
                        if virtual_selections.contains_key(&(name.clone(), version.clone())) {
                            return None;
                        }
                        if replaced_names.contains(name.as_str()) {
                            return None;
                        }
                        Some(ResolvedPackage {
                            name: name.to_string(),
                            version: version.to_string(),
                        })
                    }
                })
                .collect();
            packages.sort_by(|a, b| a.name.cmp(&b.name));
            drop(virtual_selections);
            UpdateSummary { packages, no_dev: opts.no_dev }
        }
        Err(PubGrubError::NoSolution(tree)) => {
            let detail = provider
                .analyze_resolution_problems()
                .unwrap_or_else(|| DefaultStringReporter::report(&tree));
            return Err(eyre!("no valid dependency resolution exists:\n\n{detail}"));
        }
        Err(PubGrubError::ErrorChoosingVersion { package, source }) => {
            return Err(eyre!(
                "solver could not choose a version for {package}: {}",
                source.0,
            ));
        }
        Err(PubGrubError::ErrorRetrievingDependencies {
            package,
            version,
            source,
        }) => {
            return Err(eyre!(
                "solver could not retrieve dependencies of {package}@{version}: {}",
                source.0,
            ));
        }
        Err(other) => return Err(eyre!("solver error: {other}")),
    };
    // Skip `drop_in_place<ResolveProvider>` — the process exits
    // immediately after we return, and the cached LockPackage tree
    // (~580 entries on a magento2-sized resolve) accounts for ~22%
    // of main-thread time on the profile-bench fixture. The kernel
    // reclaims the memory cleanly; nothing in the provider holds an
    // OS resource that requires Drop to release (the reqwest client
    // pools connections via its own internal runtime, which is
    // already winding down).
    std::mem::forget(provider);
    Ok(summary)
}

/// Outcome of [`resolve_for_lockfile`]: the resolved package set,
/// already partitioned into Composer's `packages` and `packages-dev`
/// arrays.
///
/// Production vs dev is determined by running the solver twice — once
/// with the full graph (`require` + `require-dev`) and once with
/// `require` only — and treating anything that disappears in the
/// prod-only solve as dev-only. Matches Composer's semantics: a
/// package reachable from `require` is production, even if it's
/// *also* reachable from `require-dev`.
///
/// Stability fields are pre-converted to Composer's wire form
/// (`minimum-stability` keyword string, `stability-flags` integer
/// constants) so callers don't need to depend on `bougie-semver` to
/// build a `Lock`.
#[derive(Debug, Clone)]
pub struct LockfileSolveOutcome {
    /// Resolved packages reachable from `composer.json`'s `require`.
    pub packages: Vec<LockPackage>,
    /// Resolved packages reachable ONLY from `require-dev`.
    pub packages_dev: Vec<LockPackage>,
    /// Composer-style keyword for the global stability floor
    /// (`"stable"`, `"dev"`, ...). Matches the lockfile's
    /// `minimum-stability` field verbatim.
    pub minimum_stability: String,
    /// Mirrors composer.json's top-level `prefer-stable`. Carried
    /// through into the lockfile's same-named field so a subsequent
    /// install / verify sees the same setting that produced the
    /// solve.
    pub prefer_stable: bool,
    /// Per-package stability flags in Composer's wire form: the
    /// integer constants used in the lockfile's `stability-flags`
    /// map. Composer's `BasePackage::STABILITIES`:
    /// stable=0, RC=5, beta=10, alpha=15, dev=20.
    pub stability_flags: std::collections::BTreeMap<String, i32>,
}

/// Resolve `composer.json` end-to-end for lockfile writing: solves
/// the full graph + a prod-only graph, then partitions the
/// difference into [`LockfileSolveOutcome::packages_dev`].
///
/// Returns the parsed composer.json bytes alongside the outcome so
/// the caller can compute content-hash without re-reading.
pub fn resolve_for_lockfile(
    paths: &Paths,
    project_root: &Path,
    default_packagist: Repo,
) -> Result<(Vec<u8>, LockfileSolveOutcome)> {
    let composer_json_path = project_root.join("composer.json");
    if !composer_json_path.is_file() {
        return Err(eyre!(
            "{} not found — not a Composer project",
            composer_json_path.display(),
        ));
    }
    let composer_json_bytes = std::fs::read(&composer_json_path)
        .wrap_err_with(|| format!("reading {}", composer_json_path.display()))?;
    let composer_json: Value = serde_json::from_slice(&composer_json_bytes)
        .map_err(|e| eyre!("parsing composer.json: {e}"))?;

    let auth = read_all_auth(&composer_json, project_root).map_err(|e| eyre!(e))?;

    let full = solve_into_lock_packages(
        paths,
        default_packagist.clone(),
        &composer_json,
        false,
        auth.clone(),
        ProgressMode::Visible,
    )?;
    // Second pass is the same closure walk against the same disk
    // cache — every fetch is an instant cache hit. Re-running the
    // spinner would flash a redundant "fetching metadata" line, so
    // hide it. The user already saw the work happen during the full
    // pass.
    let prod = solve_into_lock_packages(
        paths,
        default_packagist,
        &composer_json,
        true,
        auth,
        ProgressMode::Hidden,
    )?;

    let t_partition = std::time::Instant::now();
    let prod_names: std::collections::HashSet<&str> =
        prod.packages.iter().map(|p| p.name.as_str()).collect();
    let (packages, packages_dev): (Vec<LockPackage>, Vec<LockPackage>) = full
        .packages
        .into_iter()
        .partition(|p| prod_names.contains(p.name.as_str()));

    let stability_flags = full
        .stability_flags
        .iter()
        .map(|(name, stability)| (name.clone(), stability_to_composer_int(*stability)))
        .collect();
    tracing::info!(
        elapsed_ms = crate::elapsed_ms(t_partition.elapsed()),
        packages = packages.len(),
        packages_dev = packages_dev.len(),
        "partition_prod_dev",
    );

    Ok((
        composer_json_bytes,
        LockfileSolveOutcome {
            packages,
            packages_dev,
            minimum_stability: full.minimum_stability.as_str().to_owned(),
            prefer_stable: full.prefer_stable,
            stability_flags,
        },
    ))
}

/// Inner helper: build a provider with the given `no_dev` flag, solve,
/// pull the full `LockPackage` entries out of the cache, return them
/// sorted by name plus the metadata fields the lockfile writer needs.
fn solve_into_lock_packages(
    paths: &Paths,
    default_packagist: Repo,
    composer_json: &Value,
    no_dev: bool,
    auth: HashMap<String, crate::metadata::AuthCredentials>,
    progress: ProgressMode,
) -> Result<SolutionSummary> {
    let client = build_client()?;
    let mut provider = ResolveProvider::build_with_auth(
        client,
        paths.clone(),
        default_packagist,
        composer_json,
        no_dev,
        auth,
    )
    .map_err(|e| eyre!(e))?;
    // Record discovered Composer protocols on each repo (see
    // [`ResolveProvider::discover_repos`]). Done here too because
    // `solve_into_lock_packages` builds its own provider for the
    // full-graph and prod-only solves.
    let t_discover = std::time::Instant::now();
    provider.discover_repos();
    tracing::info!(
        elapsed_ms = crate::elapsed_ms(t_discover.elapsed()),
        repos = provider.repos.len(),
        no_dev,
        "discover_repos",
    );
    // Eager pre-fetch: load every reachable package's metadata
    // before the solver runs so virtual providers are registered.
    // Mirrors Composer's PoolBuilder.
    let t_prefetch = std::time::Instant::now();
    let prefetch = match progress {
        ProgressMode::Visible => provider.pre_fetch_closure(),
        ProgressMode::Hidden => provider.pre_fetch_closure_silent(),
    };
    prefetch.map_err(|e| eyre!("pre-fetching metadata closure: {}", e.0))?;
    tracing::info!(
        elapsed_ms = crate::elapsed_ms(t_prefetch.elapsed()),
        cached_packages = provider.cache_size(),
        no_dev,
        "pre_fetch_closure",
    );
    let root = provider.root_version();

    if matches!(progress, ProgressMode::Visible) {
        provider.begin_solve_progress();
    }
    let t_solve = std::time::Instant::now();
    let mut solve_result = resolve(&provider, PubGrubPackage::Root, root.clone());
    for _retry in 0..10 {
        let Ok(ref solution) = solve_result else { break };
        let pairs: Vec<_> = solution.iter().collect();
        let violations = provider.check_conflict_violations(&pairs);
        if violations.is_empty() {
            break;
        }
        tracing::debug!(
            count = violations.len(),
            excluded = ?violations,
            "conflict violations found, re-solving",
        );
        {
            let mut excludes = provider.conflict_excludes.borrow_mut();
            for (name, ver) in violations {
                excludes.insert((name, ver));
            }
        }
        provider.reset_for_re_solve();
        solve_result = resolve(&provider, PubGrubPackage::Root, root.clone());
    }
    let solve_elapsed = t_solve.elapsed();
    provider.finish_solve_progress();
    tracing::info!(
        elapsed_ms = crate::elapsed_ms(solve_elapsed),
        ok = solve_result.is_ok(),
        no_dev,
        "pubgrub_resolve",
    );
    let solution = match solve_result {
        Ok(s) => s,
        Err(PubGrubError::NoSolution(tree)) => {
            let detail = provider
                .analyze_resolution_problems()
                .unwrap_or_else(|| DefaultStringReporter::report(&tree));
            return Err(eyre!("no valid dependency resolution exists:\n\n{detail}"));
        }
        Err(PubGrubError::ErrorChoosingVersion { package, source }) => {
            return Err(eyre!(
                "solver could not choose a version for {package}: {}",
                source.0,
            ));
        }
        Err(PubGrubError::ErrorRetrievingDependencies {
            package,
            version,
            source,
        }) => {
            return Err(eyre!(
                "solver could not retrieve dependencies of {package}@{version}: {}",
                source.0,
            ));
        }
        Err(other) => return Err(eyre!("solver error: {other}")),
    };

    let t_assemble = std::time::Instant::now();
    let mut packages: Vec<LockPackage> = Vec::new();
    let virtual_selections = provider.virtual_selections.borrow();
    let solution_pairs: Vec<_> = solution.iter().collect();
            let replaced_names = collect_active_replaces(&provider, &solution_pairs);
    for (pkg, version) in solution {
        let PubGrubPackage::Package(name) = pkg else { continue };
        if virtual_selections.contains_key(&(name.clone(), version.clone())) {
            continue;
        }
        if replaced_names.contains(name.as_str()) {
            continue;
        }
        let Some(entry) = provider.lock_package_for(name.as_str(), &version) else {
            // Should not happen — pubgrub only picked versions we
            // returned from `choose_version`, and those entries are
            // in the cache by construction.
            return Err(eyre!(
                "internal: solver picked {name}@{version} but it's not in the metadata cache",
            ));
        };
        packages.push(entry);
    }
    drop(virtual_selections);
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    tracing::info!(
        elapsed_ms = crate::elapsed_ms(t_assemble.elapsed()),
        packages = packages.len(),
        no_dev,
        "assemble_lock_packages",
    );

    let summary = SolutionSummary {
        packages,
        minimum_stability: provider.minimum_stability,
        prefer_stable: provider.prefer_stable,
        stability_flags: provider.stability_flags.clone(),
    };
    // See `dry_run_update` for the rationale; `resolve_for_lockfile`
    // builds *two* providers (full graph + prod-only solve) so the
    // savings double here.
    std::mem::forget(provider);
    Ok(summary)
}

struct SolutionSummary {
    packages: Vec<LockPackage>,
    minimum_stability: Stability,
    prefer_stable: bool,
    stability_flags: HashMap<String, Stability>,
}

/// Composer's per-package stability constants for the lockfile's
/// `stability-flags` map. Matches
/// `Composer\Package\BasePackage::STABILITIES`.
fn stability_to_composer_int(s: Stability) -> i32 {
    match s {
        Stability::Stable => 0,
        Stability::Rc => 5,
        Stability::Beta => 10,
        Stability::Alpha => 15,
        Stability::Dev => 20,
    }
}

/// Build a set of package names that are actively replaced by another
/// package in the solution. Used by the post-solve filter to suppress
/// packages that were pulled into the graph by the replace-as-require
/// mechanism but should not appear in the final output because their
/// replacer provides them.
///
/// A replaced name is "active" when BOTH:
/// 1. The replacer is in the solution.
/// 2. The replaced name is also in the solution (pulled in by the
///    replace-as-require constraint).
///
/// Only exact-version replaces are considered (same filter as the
/// replace-as-require emission in `compute_parsed_deps`).
fn collect_active_replaces(
    provider: &ResolveProvider,
    solution: &[(&PubGrubPackage, &Version)],
) -> std::collections::HashSet<String> {
    let solution_names: std::collections::HashSet<&str> = solution
        .iter()
        .filter_map(|(pkg, _)| match pkg {
            PubGrubPackage::Package(n) => Some(n.as_str()),
            PubGrubPackage::Root => None,
        })
        .collect();

    let mut replaced: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mc = provider.merged_cache.borrow();
    for (pkg, version) in solution {
        let PubGrubPackage::Package(name) = pkg else { continue };
        let Some(cached) = mc.get(name.as_str()) else { continue };
        let Some((_, entry)) = cached.iter().find(|(v, _)| v == *version) else {
            continue;
        };
        for replaced_name in entry.replace.keys() {
            if solution_names.contains(replaced_name.as_str()) {
                replaced.insert(replaced_name.clone());
            }
        }
    }
    replaced
}

#[cfg(test)]
mod tests;
