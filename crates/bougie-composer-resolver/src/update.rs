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
//! - Streaming parse + fan-out-on-discovery prefetcher
//! - Byte-equivalence with Composer's own lockfile output (we
//!   promise semantic equivalence: composer install accepts our
//!   lock; key order and topological sort may diverge)

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use bougie_composer::lockfile::LockPackage;
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
    fetch_package_metadata_optional, fetch_package_metadata_v1_optional,
    load_v1_provider_table, probe_protocol, Repo, RepoProtocol, Variant,
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
    root_deps: Vec<(String, ComposerRange)>,
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
    cache: RefCell<HashMap<String, Vec<(Version, LockPackage)>>>,
    /// Index of virtual provider entries: maps each virtual package
    /// name (e.g. `psr/http-client-implementation`) to the list of
    /// real packages that provide or replace it. Populated by
    /// [`Self::pre_fetch_closure`] before the solve runs — matches
    /// what Composer's `PoolBuilder` does, so that when pubgrub asks
    /// `choose_version("psr/http-client-implementation", ^1)` the
    /// answer can come from the index even though Packagist has no
    /// `/p2/psr/http-client-implementation.json`.
    virtual_providers: RefCell<HashMap<String, Vec<VirtualProvider>>>,
    /// Wildcard / range-shaped replace+provide clauses. Composer's
    /// `replace: { codeception/phpunit-wrapper: "*" }` on
    /// codeception 5.x says "I replace any version of
    /// phpunit-wrapper" — Composer's `whatProvides` then satisfies
    /// any require on phpunit-wrapper from codeception's pool entry.
    /// We model this by remembering the range the provider declares
    /// and synthesizing a candidate inside the consumer's range at
    /// `choose_version` time (when the consumer's range is known).
    virtual_wildcards: RefCell<HashMap<String, Vec<WildcardProvider>>>,
    /// Reverse-lookup: for each (virtual_name, selected_version),
    /// every (provider_name, provider_version) pair that registered
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
    virtual_selections: RefCell<HashMap<(String, Version), Vec<(String, Version)>>>,
    /// Per-v1-repo merged provider lookup tables (package name →
    /// sha256). Lazily populated on the first per-package lookup
    /// against a given v1 repo, keyed by `repo.url`. Composer v1
    /// requires loading every `provider-includes` file before any
    /// package can be resolved (the includes are the only index
    /// telling us which package's hash to use); this cache makes
    /// that load happen at most once per resolve per repo.
    v1_provider_tables: RefCell<HashMap<String, HashMap<String, String>>>,
    /// Spinner ticked on every `versions_for` call so the pubgrub
    /// `resolve` phase has visible progress. Defaults to hidden;
    /// orchestrators flip it on with `begin_solve_progress` around
    /// the `resolve` call and clear it with `finish_solve_progress`.
    /// Without this the solver phase is silent — for projects with
    /// hundreds of dependencies that silence reads as a hang.
    solve_progress: RefCell<SolveProgress>,
}

/// One entry in the virtual provider index — "real package
/// `provider_name@provider_version` declares it provides/replaces
/// the virtual name at version `provided_version`."
#[derive(Debug, Clone)]
pub struct VirtualProvider {
    pub provider_name: String,
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
    pub provider_name: String,
    pub provider_version: Version,
    /// The range the provider declared it covers. For `*` this is
    /// `Ranges::full()` (matches every consumer constraint). For
    /// something like `replace: { Q: "^1.0" }` we use the parsed
    /// constraint's range.
    pub provided_range: ComposerRange,
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
        let (root_deps, stability_flags) = read_root_requires(composer_json, no_dev)?;
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
            cache: RefCell::new(HashMap::new()),
            virtual_providers: RefCell::new(HashMap::new()),
            virtual_wildcards: RefCell::new(HashMap::new()),
            virtual_selections: RefCell::new(HashMap::new()),
            v1_provider_tables: RefCell::new(HashMap::new()),
            solve_progress: RefCell::new(SolveProgress::hidden()),
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
    /// debug verbs.
    pub fn cache_size(&self) -> usize {
        self.cache.borrow().len()
    }

    /// Return the cached [`LockPackage`] for `(name, version)` if it
    /// was loaded during the current solve. Used by the lockfile
    /// writer to recover the full Packagist entry (dist, require,
    /// autoload, etc.) for each resolved version.
    pub fn lock_package_for(&self, name: &str, version: &Version) -> Option<LockPackage> {
        let cache = self.cache.borrow();
        let entries = cache.get(name)?;
        entries
            .iter()
            .find(|(v, _)| v == version)
            .map(|(_, p)| p.clone())
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
    fn versions_for(&self, name: &str) -> Result<Vec<(Version, LockPackage)>, ProviderError> {
        self.solve_progress.borrow().tick(name);
        tracing::trace!(package = %name, "versions_for");
        let floor = self.effective_stability(name);

        // The Packagist side is cached — once we've made the network
        // call for a name, we don't repeat it.
        let real_candidates = if let Some(v) = self.cache.borrow().get(name) {
            v.clone()
        } else {
            let real = self.load_real_candidates(name, floor)?;
            self.cache.borrow_mut().insert(name.to_owned(), real.clone());
            real
        };

        // Virtual candidates are merged in on every call so a
        // late-registered provider becomes visible even after the
        // first cache populate. (Pre-fetch can hit a virtual name
        // before any of its providers have been loaded.)
        let mut out: Vec<(Version, LockPackage)> = real_candidates;
        out.extend(self.synthesize_virtual_candidates(name, floor));
        out.sort_by(|a, b| b.0.cmp(&a.0));
        // Deduplicate consecutive entries with the same parsed
        // version — when both Packagist and a virtual provider
        // register the same name+version, prefer the real entry
        // (sort is stable; real Packagist entries are at the front
        // because they were extended in first).
        out.dedup_by(|a, b| a.0 == b.0);
        Ok(out)
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
                if let Some(md) = dev_md {
                    if let Some(extra) = md.packages.get(name) {
                        versions.extend(extra.iter().cloned());
                    }
                }
            }
        }

        // Register virtuals as a side effect of loading.
        self.register_virtuals_from(name, &versions);

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

    /// Walk a freshly-loaded package's versions and register every
    /// `provide` / `replace` entry in the virtual-provider index.
    /// `provider_name` is the name we just loaded metadata for.
    /// Platform names (`php`, `ext-*`, ...) are skipped — they're
    /// filtered before reaching pubgrub anyway (issue #118).
    fn register_virtuals_from(&self, provider_name: &str, versions: &[LockPackage]) {
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
                    if let Ok(v) = Version::parse(effective) {
                        index
                            .entry(virtual_name.clone())
                            .or_default()
                            .push(VirtualProvider {
                                provider_name: provider_name.to_owned(),
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
                            .entry((virtual_name.clone(), v))
                            .or_default()
                            .push((provider_name.to_owned(), provider_version.clone()));
                    } else if let Ok(constraint) = Constraint::parse(effective) {
                        wildcards
                            .entry(virtual_name.clone())
                            .or_default()
                            .push(WildcardProvider {
                                provider_name: provider_name.to_owned(),
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
                    name: name.to_owned(),
                    version: entry.provided_version.normalized.clone(),
                    version_normalized: Some(entry.provided_version.normalized.clone()),
                    dist: None,
                    source: None,
                    require: Default::default(),
                    require_dev: Default::default(),
                    package_type: Some("metapackage".into()),
                    autoload: Default::default(),
                    autoload_dev: serde_json::Value::Null,
                    replace: Default::default(),
                    provide: Default::default(),
                    conflict: Default::default(),
                    bin: Default::default(),
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
        name: &str,
        range: &ComposerRange,
    ) -> Option<Version> {
        let wildcards = self.virtual_wildcards.borrow();
        let entries = wildcards.get(name)?;
        if entries.is_empty() {
            return None;
        }
        // Deterministic order: smallest provider name first.
        let mut sorted: Vec<&WildcardProvider> = entries.iter().collect();
        sorted.sort_by(|a, b| a.provider_name.cmp(&b.provider_name));

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
                (name.to_owned(), picked.clone()),
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
        use std::collections::HashSet;
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: Vec<String> = self
            .root_deps
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        while let Some(name) = queue.pop() {
            if is_platform(&name) {
                continue;
            }
            if !visited.insert(name.clone()) {
                continue;
            }
            // Bump the spinner *before* the fetch so the displayed
            // package name reflects the one we're currently waiting
            // on, not the one we just finished.
            progress.tick(&name);
            // versions_for both loads metadata and registers
            // virtuals as a side effect.
            let versions = self.versions_for(&name)?;
            for (_, pkg) in &versions {
                for req in pkg.require.keys() {
                    if !visited.contains(req) {
                        queue.push(req.clone());
                    }
                }
            }
        }
        progress.finish();
        Ok(())
    }
}

/// Spinner that ticks once per visited package during
/// `pre_fetch_closure`, gated on the global `progress_visible` flag
/// (which the CLI flips off for `--quiet` and `--format json`).
/// Hidden in tests + JSON output by construction — `ProgressBar`
/// with a hidden draw target is a cheap no-op for every `tick` /
/// `finish` call.
///
/// The closure is invoked twice per `composer update` (once for the
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
) -> Result<(Vec<(String, ComposerRange)>, HashMap<String, Stability>), BuildError> {
    let mut out: Vec<(String, ComposerRange)> = Vec::new();
    let mut flags: HashMap<String, Stability> = HashMap::new();
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
            out.push((dep_name.clone(), to_range(&constraint)));
        }
    }
    Ok((out, flags))
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

/// Read auth credentials from composer.json's `config.http-basic`
/// and `config.bearer` maps. Composer's auth.json (project-level)
/// is merged in by the orchestrators with project_root context —
/// this function only handles the in-composer.json side.
///
/// Returns a map keyed by hostname. `http-basic` and `bearer` are
/// mutually exclusive per host; if both are declared for the same
/// host, `http-basic` wins (matches Composer's precedence).
///
/// Out of scope here: `github-oauth`, `gitlab-token`, `gitlab-oauth`
/// — those target VCS hosts which we don't fetch from yet.
pub fn read_auth_from_composer_json(
    composer_json: &Value,
) -> Result<HashMap<String, crate::metadata::AuthCredentials>, BuildError> {
    use crate::metadata::AuthCredentials;
    let mut out: HashMap<String, AuthCredentials> = HashMap::new();
    let Some(obj) = composer_json.as_object() else {
        return Ok(out);
    };
    let Some(config) = obj.get("config").and_then(Value::as_object) else {
        return Ok(out);
    };
    if let Some(bearer) = config.get("bearer").and_then(Value::as_object) {
        for (host, val) in bearer {
            let token = val.as_str().ok_or_else(|| {
                BuildError::Internal(format!(
                    "config.bearer.{host} must be a string token",
                ))
            })?;
            out.insert(host.clone(), AuthCredentials::Bearer { token: token.to_owned() });
        }
    }
    if let Some(http_basic) = config.get("http-basic").and_then(Value::as_object) {
        for (host, val) in http_basic {
            let entry = val.as_object().ok_or_else(|| {
                BuildError::Internal(format!(
                    "config.http-basic.{host} must be an object with `username` and `password`",
                ))
            })?;
            let username = entry
                .get("username")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    BuildError::Internal(format!(
                        "config.http-basic.{host}.username is missing or not a string",
                    ))
                })?;
            let password = entry
                .get("password")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    BuildError::Internal(format!(
                        "config.http-basic.{host}.password is missing or not a string",
                    ))
                })?;
            // http-basic wins over bearer for the same host
            // (mirrors Composer's precedence — http-basic is the
            // older / more explicit auth shape).
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

/// Read auth from a project-level `auth.json` (next to
/// composer.json). The shape is the same as composer.json's
/// `config` section but at the top level. Returns an empty map if
/// the file doesn't exist.
///
/// Composer reads `auth.json` in priority over composer.json's
/// `config` for the same host — the merge happens at the call
/// site by writing `auth.json` entries LAST into the combined map.
pub fn read_auth_json(
    project_root: &Path,
) -> Result<HashMap<String, crate::metadata::AuthCredentials>, BuildError> {
    use crate::metadata::AuthCredentials;
    let path = project_root.join("auth.json");
    if !path.is_file() {
        return Ok(HashMap::new());
    }
    let bytes = std::fs::read(&path).map_err(|e| {
        BuildError::Internal(format!("reading {}: {e}", path.display()))
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
        BuildError::Internal(format!("parsing {}: {e}", path.display()))
    })?;
    let mut out: HashMap<String, AuthCredentials> = HashMap::new();
    let Some(obj) = value.as_object() else {
        return Ok(out);
    };
    // `auth.json`'s shape is exactly the `config` shape but
    // top-level — http-basic / bearer maps. Reuse the parser by
    // synthesizing the wrapped form, but keeping the BuildError
    // wording focused on the file. Simpler: duplicate the small
    // parser inline rather than introducing a helper that has to
    // re-thread the error context.
    if let Some(bearer) = obj.get("bearer").and_then(Value::as_object) {
        for (host, val) in bearer {
            let token = val.as_str().ok_or_else(|| {
                BuildError::Internal(format!(
                    "{}: bearer.{host} must be a string token",
                    path.display(),
                ))
            })?;
            out.insert(host.clone(), AuthCredentials::Bearer { token: token.to_owned() });
        }
    }
    if let Some(http_basic) = obj.get("http-basic").and_then(Value::as_object) {
        for (host, val) in http_basic {
            let entry = val.as_object().ok_or_else(|| {
                BuildError::Internal(format!(
                    "{}: http-basic.{host} must be an object",
                    path.display(),
                ))
            })?;
            let username = entry
                .get("username")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    BuildError::Internal(format!(
                        "{}: http-basic.{host}.username missing",
                        path.display(),
                    ))
                })?;
            let password = entry
                .get("password")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    BuildError::Internal(format!(
                        "{}: http-basic.{host}.password missing",
                        path.display(),
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
    out: &mut Vec<(String, ComposerRange)>,
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
        out.push((dep_name.clone(), to_range(&constraint)));
    }
    Ok(())
}

impl DependencyProvider for ResolveProvider {
    type P = PubGrubPackage;
    type V = Version;
    type VS = ComposerRange;
    type Priority = ();
    type M = String;
    type Err = ProviderError;

    fn prioritize(
        &self,
        _package: &Self::P,
        _range: &Self::VS,
        _stats: &PackageResolutionStatistics,
    ) -> Self::Priority {
        // First-pass: every package is equal-priority. A refined
        // heuristic (Tsai-style "fewer candidates first") lands when
        // benchmark fixtures exist to validate the change.
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
                let versions = self.versions_for(name)?;
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
                let floor = self.effective_stability(name);
                if self.prefer_stable && floor != Stability::Stable {
                    for (v, _) in &versions {
                        if v.stability() == Stability::Stable && range.contains(v) {
                            return Ok(Some(v.clone()));
                        }
                    }
                }
                for (v, _) in &versions {
                    if range.contains(v) {
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
        let deps: Vec<(String, ComposerRange)> = match package {
            PubGrubPackage::Root => self.root_deps.clone(),
            PubGrubPackage::Package(name) => {
                // Virtual selections take priority: if pubgrub picked
                // a virtual (e.g. `psr/http-client-implementation @
                // 1.0`), the only dependency we emit is a tight pin
                // on the real providing package. That binds the
                // virtual selection to the concrete provider; the
                // provider then carries its own require chain
                // through pubgrub naturally.
                let virtual_key = (name.clone(), version.clone());
                if let Some(providers) =
                    self.virtual_selections.borrow().get(&virtual_key).cloned()
                {
                    // Group registrations by provider name. With one
                    // distinct provider we can pin to the union of
                    // its versions and pubgrub picks whichever fits
                    // the rest of the resolution. With multiple
                    // distinct providers — the classic
                    // `psr/http-client-implementation` case where
                    // guzzle, symfony/http-client, and various
                    // libraries all advertise `provide: { ...: '1.0' }`
                    // — pubgrub's `Dependencies::Available` has no
                    // OR across distinct package names, so we emit
                    // no constraint at all. The virtual still appears
                    // in the solution as a marker; the real provider
                    // is expected to be pulled in via another require
                    // edge (e.g. magento → guzzle). Composer's solver
                    // expresses this naturally as an OR-of-providers
                    // rule; bougie's modeling lacks that today, so
                    // accept the looser semantics until the resolver
                    // grows a proper provider-selector.
                    let mut by_name: std::collections::BTreeMap<String, Vec<Version>> =
                        std::collections::BTreeMap::new();
                    for (pname, pver) in &providers {
                        by_name.entry(pname.clone()).or_default().push(pver.clone());
                    }
                    let constraints: DependencyConstraints<Self::P, Self::VS> = if by_name.len() == 1
                    {
                        let (pname, versions) = by_name.iter().next().expect("non-empty");
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
                        std::iter::once((PubGrubPackage::Package(pname.clone()), range)).collect()
                    } else {
                        DependencyConstraints::default()
                    };
                    return Ok(Dependencies::Available(constraints));
                }
                let versions = self.versions_for(name)?;
                // Find the entry by reference equality on the cached
                // `Version` — same instance `choose_version` returned,
                // no re-parse, no chance of a Packagist
                // `version_normalized` quirk masquerading as a missing
                // version.
                let Some((_, entry)) = versions.iter().find(|(v, _)| v == version) else {
                    // Shouldn't happen — pubgrub only asks for
                    // dependencies of a version we just returned from
                    // `choose_version`. Surface as a hard error so a
                    // future refactor doesn't silently mask the bug.
                    return Err(ProviderError(format!(
                        "internal: get_dependencies({name}@{}) but version not in cache",
                        version,
                    )));
                };
                let mut out: Vec<(String, ComposerRange)> = Vec::new();
                let owner_version = &entry.version;
                push_constraint_map(&mut out, &entry.require, name, owner_version, "require")?;
                // `replace` is encoded as an additional require ONLY
                // when the clause is a bare version
                // (`replace: { sub: 2.0.0 }`). This emits
                // `sub: ==2.0.0` from the replacer, mirroring
                // Composer's "no coexistence" rule: selecting the
                // replacer forces the replaced package to its
                // exact replace-version (preventing real sub at any
                // other version from being selected alongside).
                //
                // For range/wildcard replaces
                // (`replace: { sub: * }`, `replace: { sub: ^1.0 }`),
                // there's no single version to pin — the replacer
                // simply offers a capability. Those flow through the
                // virtual-provider index alone; emitting them here
                // would pull a virtual name into the graph
                // unnecessarily and force a wildcard synthesis even
                // when no real consumer needs the replaced name.
                //
                // `provide` follows the same logic: capability
                // declaration via the virtual index, never an
                // additional require.
                let exact_replaces: std::collections::BTreeMap<String, String> = entry
                    .replace
                    .iter()
                    .filter(|(_, v)| {
                        let effective = if v.as_str() == "self.version" {
                            owner_version.as_str()
                        } else {
                            v.as_str()
                        };
                        Version::parse(effective).is_ok()
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                push_constraint_map(&mut out, &exact_replaces, name, owner_version, "replace")?;
                out
            }
        };
        let constraints: DependencyConstraints<Self::P, Self::VS> = deps
            .into_iter()
            .map(|(n, r)| (PubGrubPackage::Package(n), r))
            .collect();
        Ok(Dependencies::Available(constraints))
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
    // Auth assembly: start with composer.json's `config.http-basic`
    // / `config.bearer`, then merge `<project_root>/auth.json` on
    // top (auth.json wins — matches Composer's precedence).
    let mut auth = read_auth_from_composer_json(&composer_json).map_err(|e| eyre!(e))?;
    auth.extend(read_auth_json(project_root).map_err(|e| eyre!(e))?);
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
        elapsed_ms = t_discover.elapsed().as_millis() as u64,
        repos = provider.repos.len(),
        "discover_repos",
    );
    let t_prefetch = std::time::Instant::now();
    provider
        .pre_fetch_closure()
        .map_err(|e| eyre!("pre-fetching metadata closure: {}", e.0))?;
    tracing::info!(
        elapsed_ms = t_prefetch.elapsed().as_millis() as u64,
        cached_packages = provider.cache_size(),
        "pre_fetch_closure",
    );
    let root = provider.root_version();

    provider.begin_solve_progress();
    let t_solve = std::time::Instant::now();
    let result = resolve(&provider, PubGrubPackage::Root, root);
    let solve_elapsed = t_solve.elapsed();
    provider.finish_solve_progress();
    tracing::info!(
        elapsed_ms = solve_elapsed.as_millis() as u64,
        ok = result.is_ok(),
        "pubgrub_resolve",
    );
    match result {
        Ok(solution) => {
            let virtual_selections = provider.virtual_selections.borrow();
            let mut packages: Vec<ResolvedPackage> = solution
                .into_iter()
                .filter_map(|(pkg, version)| match pkg {
                    PubGrubPackage::Root => None,
                    PubGrubPackage::Package(name) => {
                        // Drop virtual selections — the real provider
                        // is in the same solution and is already
                        // accounted for.
                        if virtual_selections.contains_key(&(name.clone(), version.clone())) {
                            return None;
                        }
                        Some(ResolvedPackage {
                            name,
                            version: version.to_string(),
                        })
                    }
                })
                .collect();
            packages.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(UpdateSummary { packages, no_dev: opts.no_dev })
        }
        Err(PubGrubError::NoSolution(tree)) => Err(eyre!(
            "no valid dependency resolution exists:\n\n{}",
            DefaultStringReporter::report(&tree),
        )),
        Err(PubGrubError::ErrorChoosingVersion { package, source }) => Err(eyre!(
            "solver could not choose a version for {package}: {}",
            source.0,
        )),
        Err(PubGrubError::ErrorRetrievingDependencies {
            package,
            version,
            source,
        }) => Err(eyre!(
            "solver could not retrieve dependencies of {package}@{version}: {}",
            source.0,
        )),
        Err(other) => Err(eyre!("solver error: {other}")),
    }
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

    let mut auth = read_auth_from_composer_json(&composer_json).map_err(|e| eyre!(e))?;
    auth.extend(read_auth_json(project_root).map_err(|e| eyre!(e))?);

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
        elapsed_ms = t_partition.elapsed().as_millis() as u64,
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
        elapsed_ms = t_discover.elapsed().as_millis() as u64,
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
        elapsed_ms = t_prefetch.elapsed().as_millis() as u64,
        cached_packages = provider.cache_size(),
        no_dev,
        "pre_fetch_closure",
    );
    let root = provider.root_version();

    if matches!(progress, ProgressMode::Visible) {
        provider.begin_solve_progress();
    }
    let t_solve = std::time::Instant::now();
    let solve_result = resolve(&provider, PubGrubPackage::Root, root);
    let solve_elapsed = t_solve.elapsed();
    provider.finish_solve_progress();
    tracing::info!(
        elapsed_ms = solve_elapsed.as_millis() as u64,
        ok = solve_result.is_ok(),
        no_dev,
        "pubgrub_resolve",
    );
    let solution = match solve_result {
        Ok(s) => s,
        Err(PubGrubError::NoSolution(tree)) => {
            return Err(eyre!(
                "no valid dependency resolution exists:\n\n{}",
                DefaultStringReporter::report(&tree),
            ));
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
    for (pkg, version) in solution {
        let PubGrubPackage::Package(name) = pkg else { continue };
        // Drop virtual selections from the output — the real
        // providing package is also in the solution and will land
        // in the lock through that path. Composer's install would
        // reject a `psr/http-client-implementation` entry it can't
        // fetch.
        if virtual_selections.contains_key(&(name.clone(), version.clone())) {
            continue;
        }
        let Some(entry) = provider.lock_package_for(&name, &version) else {
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
        elapsed_ms = t_assemble.elapsed().as_millis() as u64,
        packages = packages.len(),
        no_dev,
        "assemble_lock_packages",
    );

    Ok(SolutionSummary {
        packages,
        minimum_stability: provider.minimum_stability,
        prefer_stable: provider.prefer_stable,
        stability_flags: provider.stability_flags.clone(),
    })
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

#[cfg(test)]
mod tests;
