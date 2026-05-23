//! `LockVerifyProvider`: the pubgrub [`DependencyProvider`] for
//! lock-verify. Each package has a single candidate (the version
//! named in the lock); the root package's dependencies are read
//! from `composer.json`'s `require` / `require-dev`.
//!
//! Platform requirements (`php`, `ext-*`, `lib-*`) are filtered out
//! at construction time — Phase B β doesn't encode them as
//! pubgrub packages yet. The eventual Phase C resolver will, with
//! candidates sourced from the bougie-pinned PHP + loaded
//! extensions.

use bougie_composer::lockfile::{Lock, LockPackage};

use crate::hash::FxHashMap;
use bougie_semver::constraint::Constraint;
use bougie_semver::version::Version;
use pubgrub::{Dependencies, DependencyConstraints, DependencyProvider, PackageResolutionStatistics};
use serde_json::Value;

use super::range::{to_range, ComposerRange};
use crate::package_name::PackageName;

/// pubgrub Package type. `Root` is the synthetic root that represents
/// the project itself; `Package` wraps an interned `vendor/name`
/// (see [`PackageName`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PubGrubPackage {
    Root,
    Package(PackageName),
}

impl std::fmt::Display for PubGrubPackage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Root => f.write_str("<root>"),
            Self::Package(name) => f.write_str(name.as_str()),
        }
    }
}

/// pubgrub error type. The provider only ever returns this for
/// genuinely fatal conditions (which shouldn't happen in lock-verify
/// — all data is pre-loaded). Missing-package / out-of-range cases
/// come back as `Ok(None)` from `choose_version`, which lets pubgrub
/// emit a proper derivation tree.
#[derive(Debug, Clone)]
pub struct ProviderError(pub String);

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProviderError {}

/// Read-only pubgrub provider for lock-verify.
#[derive(Debug)]
pub struct LockVerifyProvider {
    /// Locked version for each package. Built by walking
    /// `Lock::all_packages` (or `Lock::packages` if `no_dev`).
    locked: FxHashMap<PackageName, Version>,
    /// Each package's runtime dependencies, pre-converted to ranges.
    deps: FxHashMap<PackageName, Vec<(PackageName, ComposerRange)>>,
    /// Root package's dependencies (composer.json's `require`, plus
    /// `require-dev` when `no_dev` is false).
    root_deps: Vec<(PackageName, ComposerRange)>,
    /// Synthetic version pubgrub uses to identify the root.
    root_version: Version,
}

impl LockVerifyProvider {
    /// Build from a parsed lock + the raw composer.json `Value`
    /// (top-level), reading `require` and optionally `require-dev`.
    pub fn build(
        lock: &Lock,
        composer_json: &Value,
        no_dev: bool,
    ) -> Result<Self, BuildError> {
        let mut locked: FxHashMap<PackageName, Version> = FxHashMap::default();
        let mut deps: FxHashMap<PackageName, Vec<(PackageName, ComposerRange)>> =
            FxHashMap::default();
        let pkg_iter: Box<dyn Iterator<Item = &LockPackage>> = if no_dev {
            Box::new(lock.packages.iter())
        } else {
            Box::new(lock.all_packages())
        };
        for p in pkg_iter {
            let version = Version::parse(&p.version)
                .map_err(|e| BuildError::ParseVersion {
                    package: p.name.clone(),
                    version: p.version.clone(),
                    reason: e.to_string(),
                })?;
            // Intern once at the `LockPackage` boundary; this single
            // `PackageName` lives in both the `locked` and `deps`
            // entries, plus every `PubGrubPackage::Package` clone
            // pubgrub makes against it.
            let owner = PackageName::from(p.name.as_str());
            locked.insert(owner.clone(), version);

            let mut pkg_deps: Vec<(PackageName, ComposerRange)> = Vec::new();
            for (dep_name, raw_constraint) in &p.require {
                if is_platform(dep_name) {
                    continue;
                }
                let constraint = Constraint::parse(raw_constraint).map_err(|e| {
                    BuildError::ParseConstraint {
                        package: p.name.clone(),
                        dep: dep_name.clone(),
                        constraint: raw_constraint.clone(),
                        reason: e.to_string(),
                    }
                })?;
                pkg_deps.push((PackageName::from(dep_name.as_str()), to_range(&constraint)));
            }
            deps.insert(owner, pkg_deps);
        }

        // Root deps from composer.json.
        let root_deps = read_root_requires(composer_json, no_dev)?;

        // Any synthetic value works for the root version — pubgrub
        // never compares it against another candidate.
        let root_version =
            Version::parse("0.0.0.0").map_err(|e| BuildError::Internal(e.to_string()))?;

        Ok(Self { locked, deps, root_deps, root_version })
    }

    /// The synthetic root version pubgrub should `resolve` against.
    pub fn root_version(&self) -> Version {
        self.root_version.clone()
    }
}

fn read_root_requires(
    composer_json: &Value,
    no_dev: bool,
) -> Result<Vec<(PackageName, ComposerRange)>, BuildError> {
    let mut out: Vec<(PackageName, ComposerRange)> = Vec::new();
    let obj = composer_json
        .as_object()
        .ok_or_else(|| BuildError::Internal("composer.json top-level is not an object".into()))?;

    for key in if no_dev { &["require"][..] } else { &["require", "require-dev"][..] } {
        let Some(reqs) = obj.get(*key).and_then(Value::as_object) else { continue };
        for (dep_name, raw) in reqs {
            if is_platform(dep_name) {
                continue;
            }
            let raw_constraint = raw
                .as_str()
                .ok_or_else(|| BuildError::Internal(format!("{key}.{dep_name} is not a string")))?;
            let constraint = Constraint::parse(raw_constraint).map_err(|e| {
                BuildError::ParseConstraint {
                    package: "<root>".into(),
                    dep: dep_name.clone(),
                    constraint: raw_constraint.to_owned(),
                    reason: e.to_string(),
                }
            })?;
            out.push((PackageName::from(dep_name.as_str()), to_range(&constraint)));
        }
    }
    Ok(out)
}

/// Composer platform-package detection. Phase B β skips these; the
/// resolver in Phase C will encode them as proper pubgrub packages.
pub fn is_platform(name: &str) -> bool {
    name == "php"
        || name == "hhvm"
        || name == "composer"
        || name == "composer-plugin-api"
        || name == "composer-runtime-api"
        || name.starts_with("ext-")
        || name.starts_with("lib-")
        || name.starts_with("php-")
}

#[derive(Debug, Clone)]
pub enum BuildError {
    ParseVersion {
        package: String,
        version: String,
        reason: String,
    },
    ParseConstraint {
        package: String,
        dep: String,
        constraint: String,
        reason: String,
    },
    Internal(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseVersion { package, version, reason } => write!(
                f,
                "lock package `{package}` has unparseable version {version:?}: {reason}",
            ),
            Self::ParseConstraint { package, dep, constraint, reason } => write!(
                f,
                "constraint {constraint:?} on `{dep}` (from `{package}`) is invalid: {reason}",
            ),
            Self::Internal(s) => write!(f, "internal error: {s}"),
        }
    }
}

impl std::error::Error for BuildError {}

impl DependencyProvider for LockVerifyProvider {
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
        // Single-candidate lock-verify — order doesn't matter.
    }

    fn choose_version(
        &self,
        package: &Self::P,
        range: &Self::VS,
    ) -> Result<Option<Self::V>, Self::Err> {
        let candidate = match package {
            PubGrubPackage::Root => &self.root_version,
            PubGrubPackage::Package(name) => match self.locked.get(name) {
                Some(v) => v,
                // Package referenced by a require but absent from
                // the lock → no candidate. pubgrub will produce a
                // derivation tree pointing at the offending require.
                None => return Ok(None),
            },
        };
        Ok(if range.contains(candidate) {
            Some(candidate.clone())
        } else {
            None
        })
    }

    fn get_dependencies(
        &self,
        package: &Self::P,
        _version: &Self::V,
    ) -> Result<Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        let deps: &[(PackageName, ComposerRange)] = match package {
            PubGrubPackage::Root => &self.root_deps,
            PubGrubPackage::Package(name) => self
                .deps
                .get(name)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        };
        let constraints: DependencyConstraints<Self::P, Self::VS> = deps
            .iter()
            .map(|(n, r)| (PubGrubPackage::Package(n.clone()), r.clone()))
            .collect();
        Ok(Dependencies::Available(constraints))
    }
}
