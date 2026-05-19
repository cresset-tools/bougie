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
//! `replace` and `provide` are encoded by `get_dependencies` as
//! additional constraints — selecting `P@V` forces every named
//! alternative to satisfy `P`'s declared clause for it. The
//! semantic distinction Composer draws between the two (replace =
//! exclusive swap, provide = weaker capability) is invisible in
//! plain pubgrub requires; both collapse to "if P is selected,
//! then Q must match this range." The lockfile writer is
//! responsible for de-duping the install set when both `P` and
//! the replaced `Q` end up in the solution. See the comment in
//! `get_dependencies` for the known edge cases.
//!
//! Out of scope (follow-ups):
//! - `minimum-stability` / `prefer-stable` / per-package stability
//!   flags (currently hard-coded to "stable-only candidates")
//! - Platform-package version checks against the bougie-pinned PHP
//!   (issue #118 — affects polyfill-style `replace: { php: "..." }`
//!   declarations too)
//! - Custom repositories beyond Packagist
//! - The dev variant (`/p2/<name>~dev.json`) — only the stable
//!   document is consulted right now
//! - Streaming parse + fan-out-on-discovery prefetcher
//! - Lockfile writer (the `--dry-run` orchestrator ships in this PR)

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use bougie_composer::lockfile::LockPackage;
use bougie_composer::metadata::PackageMetadata;
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

use crate::metadata::{fetch_package_metadata, Variant};
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
    base_url: String,
    root_deps: Vec<(String, ComposerRange)>,
    root_version: Version,
    /// `vendor/name` → versions list, newest-first, stable-only.
    /// Populated lazily on first `choose_version` / `get_dependencies`
    /// for that package.
    cache: RefCell<HashMap<String, Vec<LockPackage>>>,
}

impl std::fmt::Debug for ResolveProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolveProvider")
            .field("base_url", &self.base_url)
            .field("root_deps", &self.root_deps)
            .field("cache_keys", &self.cache.borrow().keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ResolveProvider {
    /// Build from the parsed `composer.json` (top-level `Value`),
    /// reading `require` and optionally `require-dev`. Network access
    /// is deferred until pubgrub asks for a candidate.
    pub fn build(
        client: reqwest::blocking::Client,
        paths: Paths,
        base_url: String,
        composer_json: &Value,
        no_dev: bool,
    ) -> Result<Self, BuildError> {
        let root_deps = read_root_requires(composer_json, no_dev)?;
        // Any synthetic value works for the root version — pubgrub
        // never compares it against another candidate. Match the
        // verify provider's choice for cross-module consistency.
        let root_version =
            Version::parse("0.0.0.0").map_err(|e| BuildError::Internal(e.to_string()))?;
        Ok(Self {
            client,
            paths,
            base_url,
            root_deps,
            root_version,
            cache: RefCell::new(HashMap::new()),
        })
    }

    /// The synthetic root version pubgrub should `resolve` against.
    pub fn root_version(&self) -> Version {
        self.root_version.clone()
    }

    /// Inspect what's been fetched so far. Exposed for tests + future
    /// debug verbs.
    pub fn cache_size(&self) -> usize {
        self.cache.borrow().len()
    }

    /// Fetch (or read from cache) the version list for one package.
    /// Filters out non-stable candidates and packages whose
    /// version-string fails to parse.
    fn versions_for(&self, name: &str) -> Result<Vec<LockPackage>, ProviderError> {
        if let Some(v) = self.cache.borrow().get(name) {
            return Ok(v.clone());
        }
        let md: PackageMetadata = fetch_package_metadata(
            &self.client,
            &self.paths,
            &self.base_url,
            name,
            Variant::Stable,
        )
        .map_err(|e| ProviderError(format!("fetching metadata for {name}: {e:#}")))?;

        // Packagist's response always carries exactly one entry under
        // `packages` keyed by the requested name; defensively handle
        // the missing-key shape rather than indexing.
        let versions = md.packages.get(name).cloned().unwrap_or_default();
        let stable: Vec<LockPackage> = versions
            .into_iter()
            .filter(|p| {
                Version::parse(&p.version)
                    .map(|v| v.stability() == Stability::Stable)
                    .unwrap_or(false)
            })
            .collect();
        self.cache.borrow_mut().insert(name.to_owned(), stable.clone());
        Ok(stable)
    }
}

fn read_root_requires(
    composer_json: &Value,
    no_dev: bool,
) -> Result<Vec<(String, ComposerRange)>, BuildError> {
    let mut out: Vec<(String, ComposerRange)> = Vec::new();
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
            let constraint = Constraint::parse(raw_constraint).map_err(|e| {
                BuildError::ParseConstraint {
                    dep: dep_name.clone(),
                    constraint: raw_constraint.to_owned(),
                    reason: e.to_string(),
                }
            })?;
            out.push((dep_name.clone(), to_range(&constraint)));
        }
    }
    Ok(out)
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
fn push_constraint_map(
    out: &mut Vec<(String, ComposerRange)>,
    map: &std::collections::BTreeMap<String, String>,
    owner: &str,
    clause_kind: &'static str,
) -> Result<(), ProviderError> {
    for (dep_name, raw) in map {
        if is_platform(dep_name) {
            continue;
        }
        let constraint = Constraint::parse(raw).map_err(|e| {
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
                // Packagist orders newest-first, so the first entry
                // that falls inside the range is also the highest
                // candidate — no explicit sort needed.
                for p in &versions {
                    if let Ok(v) = Version::parse(&p.version) {
                        if range.contains(&v) {
                            return Ok(Some(v));
                        }
                    }
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
                let versions = self.versions_for(name)?;
                let entry = versions
                    .iter()
                    .find(|p| Version::parse(&p.version).ok().as_ref() == Some(version));
                let Some(entry) = entry else {
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
                push_constraint_map(&mut out, &entry.require, name, "require")?;
                // `replace` and `provide` get encoded as additional
                // requires: selecting this package forces the named
                // alternative to satisfy the replace/provide
                // constraint. The semantic distinction Composer
                // draws — "replace = strict swap, only one wins"
                // versus "provide = weaker capability assertion" —
                // is invisible in plain pubgrub requires. Both
                // collapse to "if you pick P@V, then Q must be in
                // <clause>". This is correct for the common case
                // of a project requiring both the replacer and the
                // replaced; the lockfile writer is responsible for
                // de-duping the install set so we don't extract
                // the same code twice.
                //
                // Two known limitations, both documented:
                // - Platform replaces (`replace: { php: "8.0.x" }`
                //   on polyfills) still skip — platform packages
                //   are filtered before they reach pubgrub. Tracked
                //   in issue #118.
                // - Monolith replacers (a package replacing one
                //   that has no standalone Packagist entry) will
                //   fail resolution. Rare in practice; surfaces as
                //   a fixture failure if it bites.
                push_constraint_map(&mut out, &entry.replace, name, "replace")?;
                push_constraint_map(&mut out, &entry.provide, name, "provide")?;
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
/// metadata host at `base_url` and return the package set that would
/// land in `composer.lock`.
///
/// Read-only: doesn't write `composer.lock`, doesn't touch `vendor/`.
/// The lockfile writer is a follow-up PR; until then this is the
/// only user-visible entry point into the resolver from the CLI.
///
/// `base_url` is the Packagist host (no trailing slash). Production
/// callers pass [`crate::metadata::base_url()`]; tests pass a
/// wiremock URI.
pub fn dry_run_update(
    paths: &Paths,
    project_root: &Path,
    base_url: &str,
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
    let provider = ResolveProvider::build(
        client,
        paths.clone(),
        base_url.to_owned(),
        &composer_json,
        opts.no_dev,
    )
    .map_err(|e| eyre!(e))?;
    let root = provider.root_version();

    match resolve(&provider, PubGrubPackage::Root, root) {
        Ok(solution) => {
            let mut packages: Vec<ResolvedPackage> = solution
                .into_iter()
                .filter_map(|(pkg, version)| match pkg {
                    PubGrubPackage::Root => None,
                    PubGrubPackage::Package(name) => Some(ResolvedPackage {
                        name,
                        version: version.to_string(),
                    }),
                })
                .collect();
            packages.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(UpdateSummary { packages, no_dev: opts.no_dev })
        }
        Err(PubGrubError::NoSolution(tree)) => Err(eyre!(
            "no valid dependency resolution exists:\n\n{}",
            DefaultStringReporter::report(&tree),
        )),
        Err(other) => Err(eyre!("solver error: {other}")),
    }
}

#[cfg(test)]
mod tests;
