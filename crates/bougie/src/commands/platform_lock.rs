//! Resolve the current project's toolchain into a committed `bougie.lock`.

use bougie_config::{ProjectConfig, load_project};
#[cfg(unix)]
use bougie_daemon::daemon::catalog;
use bougie_index::fetch::{fetch_manifest, fetch_root, fetch_section};
use bougie_index::{Manifest, Root, Section, build_verifier, host_to_dirname};
use bougie_installer::baseline::{self, BASELINE_EXTENSIONS};
use bougie_installer::install::DEFAULT_INDEX_URL;
use bougie_lock::{
    ArtifactDigest, ExtensionOrigin, ExtensionPin, FORMAT_VERSION, PhpPin, ServicePin,
    TargetArtifacts, ToolchainLock,
};
use bougie_paths::Paths;
use bougie_platform::target::Triple;
use bougie_resolver::{ResolveOptions, resolve_extension_constraint, resolve_php};
use bougie_version::matches::version_satisfies;
use bougie_version::request::Flavor;
use bougie_version::request::VersionLike;
use bougie_version::version::{PartialVersion, Version};
use eyre::{Result, WrapErr, eyre};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// One signed index root plus everything needed to fetch sections and
/// manifests from it. Opened once per command so every target shares a
/// single root fetch.
pub(crate) struct IndexCtx {
    client: reqwest::blocking::Client,
    host: String,
    cache_root: PathBuf,
    pub root: Root,
}

impl IndexCtx {
    pub fn open(paths: &Paths) -> Result<Self> {
        let host = std::env::var("BOUGIE_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.into());
        let client = bougie_fetch::default_client()?;
        let cache_root = paths.cache_index(&host_to_dirname(&host));
        let root = fetch_root(&client, &host, &cache_root, build_verifier)?.root;
        Ok(Self {
            client,
            host,
            cache_root,
            root,
        })
    }

    fn section(&self, target: &str, name: &str) -> Result<Section> {
        let target_entry = self.root.targets.get(target).ok_or_else(|| {
            eyre!(
                "the index at {} does not serve {target}; available targets: {}",
                self.host,
                self.root
                    .targets
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        let section_ref = target_entry.sections.get(name).ok_or_else(|| {
            eyre!(
                "the index at {} has no `{name}` section under target {target}",
                self.host
            )
        })?;
        fetch_section(
            &self.client,
            &self.host,
            &self.cache_root,
            &self.root.version,
            target,
            name,
            &section_ref.sha256,
        )
    }

    /// Fetch and validate a manifest, returning the digest pair a lock
    /// records for it.
    fn digest(&self, manifest_ref: &bougie_index::wire::ManifestRef) -> Result<ArtifactDigest> {
        let manifest: Manifest = fetch_manifest(
            &self.client,
            &self.host,
            &self.cache_root,
            &manifest_ref.path,
            &manifest_ref.sha256,
        )?;
        manifest.validate()?;
        Ok(ArtifactDigest {
            manifest_sha256: manifest_ref.sha256.clone(),
            blob_sha256: manifest.blob.sha256,
        })
    }
}

/// Which locked pins a re-lock may move, as uv's `--upgrade` and
/// `--upgrade-package`. Without either, `bougie lock --with-platform`
/// keeps every pin that still satisfies its constraint and only resolves
/// what is new or no longer allowed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Upgrade {
    #[default]
    None,
    All,
    /// Entries by name: `php`, an extension (`redis`, or `ext-redis`), or
    /// a service.
    Only(BTreeSet<String>),
}

impl Upgrade {
    pub fn from_flags(all: bool, packages: Vec<String>) -> Self {
        if all {
            Self::All
        } else if packages.is_empty() {
            Self::None
        } else {
            Self::Only(
                packages
                    .into_iter()
                    .map(|name| {
                        name.strip_prefix("ext-")
                            .map_or(name.clone(), str::to_owned)
                    })
                    .collect(),
            )
        }
    }

    fn allows(&self, name: &str) -> bool {
        match self {
            Self::None => false,
            Self::All => true,
            Self::Only(names) => names.contains(name),
        }
    }

    /// Names passed to `--upgrade-package` that match nothing in `lock`.
    pub(crate) fn unknown_names(&self, lock: &ToolchainLock) -> Vec<String> {
        let Self::Only(names) = self else {
            return Vec::new();
        };
        names
            .iter()
            .filter(|name| {
                name.as_str() != "php"
                    && !lock.extensions.contains_key(*name)
                    && !lock.services.contains_key(*name)
            })
            .cloned()
            .collect()
    }
}

/// The pins a re-lock should try to keep: the previous lock, minus
/// whatever the upgrade policy releases.
#[derive(Clone, Copy)]
pub(crate) struct Preferences<'a> {
    previous: Option<&'a ToolchainLock>,
    upgrade: &'a Upgrade,
}

impl<'a> Preferences<'a> {
    pub(crate) fn new(previous: Option<&'a ToolchainLock>, upgrade: &'a Upgrade) -> Self {
        Self { previous, upgrade }
    }

    fn php(&self) -> Option<&'a PhpPin> {
        self.previous
            .filter(|_| !self.upgrade.allows("php"))
            .map(|lock| &lock.php)
    }

    fn extension(&self, name: &str) -> Option<&'a ExtensionPin> {
        self.previous
            .filter(|_| !self.upgrade.allows(name))
            .and_then(|lock| lock.extensions.get(name))
    }

    #[cfg(unix)]
    fn service(&self, name: &str) -> Option<&'a ServicePin> {
        self.previous
            .filter(|_| !self.upgrade.allows(name))
            .and_then(|lock| lock.services.get(name))
    }
}

/// An exact `X.Y.Z` request for `version`.
fn exact_spec(version: Version) -> VersionLike {
    VersionLike::Version(PartialVersion {
        major: version.major,
        minor: Some(version.minor),
        patch: Some(version.patch),
    })
}

/// Whether a Composer constraint string admits `version`.
fn constraint_admits(constraint: &str, version: &str) -> bool {
    let constraint = constraint.trim();
    if constraint.is_empty() || constraint == "*" || constraint == version {
        return true;
    }
    composer_semver::Constraint::parse(constraint).is_ok_and(|constraint| {
        composer_semver::Version::parse(version).is_ok_and(|version| constraint.matches(&version))
    })
}

fn target_is_linux(target: &str) -> bool {
    target.split('-').any(|part| part == "linux")
}

fn target_is_windows(target: &str) -> bool {
    target.split('-').any(|part| part == "windows")
}

/// Whether a locked extension ships an artifact for `target`. The
/// portable layer lists the same extensions on every OS; a baseline
/// extension the target's platform cannot build (gettext off Linux) is
/// simply absent from that target's digest block and never installed
/// there.
pub(crate) fn extension_expected_on(name: &str, origin: ExtensionOrigin, target: &str) -> bool {
    origin != ExtensionOrigin::Baseline || !baseline::skip_for_os(name, target_is_linux(target))
}

/// Whether services are locked for `target`. The service daemon is
/// Unix-only, so a Windows target never carries service digests.
pub(crate) fn services_expected_on(target: &str) -> bool {
    !target_is_windows(target)
}

fn parse_flavor(flavor: &str) -> Result<Flavor> {
    Ok(match flavor {
        "nts" => Flavor::Nts,
        "nts-debug" => Flavor::NtsDebug,
        "zts" => Flavor::Zts,
        "zts-debug" => Flavor::ZtsDebug,
        other => return Err(eyre!("bougie.lock has unknown PHP flavor {other:?}")),
    })
}

/// Resolve the project's toolchain into a lock covering the current
/// target. Like `uv lock`, a pin from the previous lock is kept while it
/// still satisfies its constraint and is still published, unless
/// `prefer` releases it for upgrade; only new or invalidated entries
/// resolve to the newest version. On Windows, where services cannot be
/// resolved, the previous service pins are carried forward instead of
/// being dropped.
pub(crate) fn resolve(
    project_root: &Path,
    ctx: &IndexCtx,
    prefer: Preferences<'_>,
) -> Result<ToolchainLock> {
    let project = load_project(project_root)?;
    let (php_spec, flavor) = super::sync::project_php_inputs(project_root, &project)?;
    let target = Triple::detect()?.to_string();

    let php_section = ctx.section(&target, "interpreter/php")?;
    let kept_php = prefer
        .php()
        .filter(|pin| pin.flavor == flavor.to_string())
        .and_then(|pin| pin.version.parse::<Version>().ok())
        .filter(|version| version_satisfies(version, &php_spec))
        .and_then(|version| {
            resolve_php(
                &php_section,
                &exact_spec(version),
                flavor,
                ResolveOptions::default(),
            )
            .ok()
        });
    let selected = match kept_php {
        Some(selected) => selected,
        None => resolve_php(&php_section, &php_spec, flavor, ResolveOptions::default())?,
    };
    let php_version = selected.version;
    let php_digest = ctx.digest(&selected.artifact.manifest)?;

    let php_minor = PartialVersion {
        major: php_version.major,
        minor: Some(php_version.minor),
        patch: None,
    };
    let mut extension_pins = BTreeMap::new();
    let mut extension_digests = BTreeMap::new();
    for (name, input) in extension_inputs(project_root, &project, php_minor) {
        // An extension this host cannot build is still part of the
        // portable layer: resolve its version against a target that
        // ships it, and record no digest for this one.
        let on_host = extension_expected_on(&name, input.origin, &target);
        let resolve_on = if on_host {
            target.clone()
        } else {
            ctx.root
                .targets
                .keys()
                .find(|candidate| extension_expected_on(&name, input.origin, candidate))
                .cloned()
                .ok_or_else(|| eyre!("no target in the index ships extension {name}"))?
        };
        let section = ctx.section(&resolve_on, &format!("extension/{name}"))?;
        let constraint =
            composer_semver::Constraint::parse(&input.constraint).map_err(|error| {
                eyre!(
                    "extension {name} constraint {:?}: {error}",
                    input.constraint
                )
            })?;
        // The kept pin must also exist for the (possibly new) PHP minor.
        let kept = prefer
            .extension(&name)
            .filter(|pin| pin.origin == input.origin)
            .filter(|pin| {
                composer_semver::Version::parse(&pin.version)
                    .is_ok_and(|version| constraint.matches(&version))
            })
            .and_then(|pin| {
                bougie_resolver::resolve_extension(
                    &section,
                    php_minor,
                    flavor,
                    Some(&pin.version),
                    ResolveOptions::default(),
                )
                .ok()
            });
        let selected = match kept {
            Some(selected) => selected,
            None => resolve_extension_constraint(
                &section,
                php_minor,
                flavor,
                &constraint,
                ResolveOptions::default(),
            )
            .wrap_err_with(|| format!("resolving extension {name}"))?,
        };
        if on_host {
            extension_digests.insert(name.clone(), ctx.digest(&selected.artifact.manifest)?);
        }
        extension_pins.insert(
            name,
            ExtensionPin {
                constraint: input.constraint,
                version: selected.version.to_string(),
                origin: input.origin,
            },
        );
    }

    let (service_pins, service_digests) = resolve_services(&project, ctx, &target, prefer)?;

    Ok(ToolchainLock {
        version: FORMAT_VERSION,
        snapshot: ctx.root.version.clone(),
        php: PhpPin {
            constraint: php_constraint(project_root, &project, php_version),
            version: php_version.to_string(),
            flavor: flavor.to_string(),
        },
        extensions: extension_pins,
        services: service_pins,
        targets: BTreeMap::from([(
            target,
            TargetArtifacts {
                php: php_digest,
                extensions: extension_digests,
                services: service_digests,
            },
        )]),
    })
}

#[cfg(unix)]
fn resolve_services(
    project: &ProjectConfig,
    ctx: &IndexCtx,
    target: &str,
    prefer: Preferences<'_>,
) -> Result<(
    BTreeMap<String, ServicePin>,
    BTreeMap<String, ArtifactDigest>,
)> {
    let mut pins = BTreeMap::new();
    let mut digests = BTreeMap::new();
    for (name, pin) in &project.bougie.services {
        let entry = catalog::find(name)
            .ok_or_else(|| eyre!("service `{name}` is not in this bougie service catalog"))?;
        let constraint = pin.version().unwrap_or("*");
        let kept = prefer.service(name).filter(|locked| {
            constraint_admits(constraint, &locked.version)
                && (entry.version == locked.version
                    || entry.versions.contains(&locked.version.as_str()))
        });
        let version = match kept {
            Some(locked) => locked.version.clone(),
            None => super::service::up::resolve_service_version(name, pin)?,
        };
        pins.insert(
            name.clone(),
            ServicePin {
                constraint: constraint.to_owned(),
                version: version.clone(),
            },
        );
        if entry.tarball.is_empty() {
            continue;
        }
        digests.insert(
            name.clone(),
            service_digest(ctx, target, name, &version, false)?,
        );
    }
    Ok((pins, digests))
}

/// Windows has no service daemon, so it cannot resolve service versions.
/// Keep the previous lock's pins for services whose constraint is
/// unchanged, so locking on Windows does not erase what a Unix machine
/// recorded; anything else has to be locked on macOS or Linux.
#[cfg(not(unix))]
fn resolve_services(
    project: &ProjectConfig,
    _ctx: &IndexCtx,
    _target: &str,
    prefer: Preferences<'_>,
) -> Result<(
    BTreeMap<String, ServicePin>,
    BTreeMap<String, ArtifactDigest>,
)> {
    let mut pins = BTreeMap::new();
    for (name, pin) in &project.bougie.services {
        let constraint = pin.version().unwrap_or("*");
        // Windows cannot resolve a service, so even an upgrade keeps the
        // previous pin rather than dropping it.
        match prefer.previous.and_then(|lock| lock.services.get(name)) {
            Some(locked) if locked.constraint == constraint => {
                pins.insert(name.clone(), locked.clone());
            }
            _ => eprintln!(
                "warning: service {name} cannot be locked on Windows; run `bougie lock --with-platform` on macOS or Linux to pin it"
            ),
        }
    }
    Ok((pins, BTreeMap::new()))
}

#[cfg(unix)]
fn service_digest(
    ctx: &IndexCtx,
    target: &str,
    name: &str,
    version: &str,
    allow_yanked: bool,
) -> Result<ArtifactDigest> {
    let section = ctx.section(target, &format!("tool/{name}"))?;
    let artifact = section
        .artifacts
        .iter()
        .find(|artifact| artifact.version == version && (allow_yanked || !artifact.yanked))
        .ok_or_else(|| {
            eyre!(
                "the index at {} does not publish service `{name}` at {version} for {target}",
                ctx.host
            )
        })?;
    ctx.digest(&artifact.manifest)
}

/// Record the digests for `target` of exactly the versions `lock` pins.
/// Used to lazily add the current target on sync and to fill every
/// target for `bougie lock --with-platform --all-targets`.
pub(crate) fn resolve_target(
    ctx: &IndexCtx,
    lock: &ToolchainLock,
    target: &str,
) -> Result<TargetArtifacts> {
    let flavor = parse_flavor(&lock.php.flavor)?;
    let php_version = lock.php.version.parse::<Version>()?;
    let php_spec = exact_spec(php_version);
    let php_section = ctx.section(target, "interpreter/php")?;
    let selected = resolve_php(
        &php_section,
        &php_spec,
        flavor,
        ResolveOptions { allow_yanked: true },
    )?;
    let php = ctx.digest(&selected.artifact.manifest)?;

    let php_minor = PartialVersion {
        major: php_version.major,
        minor: Some(php_version.minor),
        patch: None,
    };
    let mut extensions = BTreeMap::new();
    for (name, pin) in &lock.extensions {
        if !extension_expected_on(name, pin.origin, target) {
            continue;
        }
        let section = ctx.section(target, &format!("extension/{name}"))?;
        let selected = bougie_resolver::resolve_extension(
            &section,
            php_minor,
            flavor,
            Some(&pin.version),
            ResolveOptions { allow_yanked: true },
        )?;
        extensions.insert(name.clone(), ctx.digest(&selected.artifact.manifest)?);
    }

    Ok(TargetArtifacts {
        php,
        extensions,
        services: target_service_digests(ctx, lock, target)?,
    })
}

#[cfg(unix)]
fn target_service_digests(
    ctx: &IndexCtx,
    lock: &ToolchainLock,
    target: &str,
) -> Result<BTreeMap<String, ArtifactDigest>> {
    let mut services = BTreeMap::new();
    if !services_expected_on(target) {
        return Ok(services);
    }
    for (name, pin) in &lock.services {
        if catalog::find(name).is_some_and(|entry| entry.tarball.is_empty()) {
            continue;
        }
        services.insert(
            name.clone(),
            service_digest(ctx, target, name, &pin.version, true)?,
        );
    }
    Ok(services)
}

/// A Windows host has no service catalog to tell which services ship an
/// artifact, so it records none; a Unix machine fills them in when it
/// first syncs.
#[cfg(not(unix))]
fn target_service_digests(
    _ctx: &IndexCtx,
    _lock: &ToolchainLock,
    _target: &str,
) -> Result<BTreeMap<String, ArtifactDigest>> {
    Ok(BTreeMap::new())
}

/// `true` when two locks pin the same toolchain: same PHP, extensions and
/// services at the same versions. Snapshot and digests are ignored.
fn same_pins(a: &ToolchainLock, b: &ToolchainLock) -> bool {
    a.php == b.php && a.extensions == b.extensions && a.services == b.services
}

/// Fold an existing lock into a freshly resolved one. When the pins did
/// not move, keep the old snapshot and every other target's digests, so
/// re-locking on one OS does not discard what another OS recorded.
/// Digests are tied to versions, not snapshots, so they stay valid. When
/// the pins moved, the other targets' digests are stale and are dropped;
/// [`describe_changes`] reports them.
pub(crate) fn merge_previous(lock: &mut ToolchainLock, previous: &ToolchainLock) {
    if !same_pins(lock, previous) {
        return;
    }
    lock.snapshot.clone_from(&previous.snapshot);
    for (target, artifacts) in &previous.targets {
        lock.targets
            .entry(target.clone())
            .or_insert_with(|| artifacts.clone());
    }
}

/// Human-readable per-entry changes between two locks, for
/// `bougie lock --with-platform` output.
pub(crate) fn describe_changes(
    previous: Option<&ToolchainLock>,
    lock: &ToolchainLock,
) -> Vec<String> {
    let Some(previous) = previous else {
        return Vec::new();
    };
    let mut changes = Vec::new();
    if previous.php.version != lock.php.version || previous.php.flavor != lock.php.flavor {
        changes.push(format!(
            "php {} ({}) -> {} ({})",
            previous.php.version, previous.php.flavor, lock.php.version, lock.php.flavor
        ));
    }
    diff_versions(
        "extension",
        &previous
            .extensions
            .iter()
            .map(|(name, pin)| (name.as_str(), pin.version.as_str()))
            .collect(),
        &lock
            .extensions
            .iter()
            .map(|(name, pin)| (name.as_str(), pin.version.as_str()))
            .collect(),
        &mut changes,
    );
    diff_versions(
        "service",
        &previous
            .services
            .iter()
            .map(|(name, pin)| (name.as_str(), pin.version.as_str()))
            .collect(),
        &lock
            .services
            .iter()
            .map(|(name, pin)| (name.as_str(), pin.version.as_str()))
            .collect(),
        &mut changes,
    );
    for target in previous.targets.keys() {
        if !lock.targets.contains_key(target) {
            changes.push(format!("- digests for {target}"));
        }
    }
    for target in lock.targets.keys() {
        if !previous.targets.contains_key(target) {
            changes.push(format!("+ digests for {target}"));
        }
    }
    changes
}

fn diff_versions(
    kind: &str,
    before: &BTreeMap<&str, &str>,
    after: &BTreeMap<&str, &str>,
    changes: &mut Vec<String>,
) {
    for (name, version) in before {
        match after.get(name) {
            None => changes.push(format!("- {kind} {name} {version}")),
            Some(new) if new != version => {
                changes.push(format!("{kind} {name} {version} -> {new}"));
            }
            Some(_) => {}
        }
    }
    for (name, version) in after {
        if !before.contains_key(name) {
            changes.push(format!("+ {kind} {name} {version}"));
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExtensionInput {
    pub constraint: String,
    pub origin: ExtensionOrigin,
}

/// The extension half of the portable layer. Deliberately independent of
/// the host OS: a baseline extension one platform cannot build is still
/// listed, and [`extension_expected_on`] decides per target whether it
/// carries a digest. Otherwise a lock written on Linux would look stale
/// on macOS and vice versa.
pub(crate) fn extension_inputs(
    project_root: &Path,
    project: &ProjectConfig,
    php_minor: PartialVersion,
) -> BTreeMap<String, ExtensionInput> {
    let mut inputs = BTreeMap::new();
    let composer_constraints = composer_extension_constraints(project_root);
    for name in BASELINE_EXTENSIONS {
        let disabled = project
            .bougie
            .extensions
            .get(*name)
            .is_some_and(bougie_config::ExtensionPin::is_disabled);
        if !disabled
            && !baseline::is_builtin(name)
            && !baseline::skip_for_php_minor(name, php_minor)
        {
            inputs.insert(
                (*name).to_owned(),
                ExtensionInput {
                    constraint: "*".into(),
                    origin: ExtensionOrigin::Baseline,
                },
            );
        }
    }

    for name in super::infer_php::infer_extensions(project_root).0 {
        if project
            .bougie
            .extensions
            .get(&name)
            .is_some_and(bougie_config::ExtensionPin::is_disabled)
        {
            continue;
        }
        let constraint = project
            .bougie
            .extensions
            .get(&name)
            .and_then(bougie_config::ExtensionPin::as_version)
            .unwrap_or("*")
            .to_owned();
        inputs.insert(
            name,
            ExtensionInput {
                constraint,
                origin: ExtensionOrigin::Inferred,
            },
        );
    }

    for (name, pin) in &project.bougie.extensions {
        if let Some(version) = pin.as_version() {
            inputs.insert(
                name.clone(),
                ExtensionInput {
                    constraint: version.to_owned(),
                    origin: ExtensionOrigin::Declared,
                },
            );
        }
    }
    if let Some(composer) = &project.composer {
        for name in &composer.require_extensions {
            let constraint = project
                .bougie
                .extensions
                .get(name)
                .and_then(bougie_config::ExtensionPin::as_version)
                .map(str::to_owned)
                .or_else(|| composer_constraints.get(name).cloned())
                .unwrap_or_else(|| "*".into());
            inputs.insert(
                name.clone(),
                ExtensionInput {
                    constraint,
                    origin: ExtensionOrigin::Declared,
                },
            );
        }
    }
    inputs
}

fn composer_extension_constraints(project_root: &Path) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(project_root.join("composer.json")) else {
        return BTreeMap::new();
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
        return BTreeMap::new();
    };
    root.get("require")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, constraint)| {
            Some((
                name.strip_prefix("ext-")?.to_owned(),
                constraint.as_str()?.to_owned(),
            ))
        })
        .collect()
}

fn php_constraint(
    project_root: &Path,
    project: &ProjectConfig,
    resolved: bougie_version::version::Version,
) -> String {
    php_constraint_input(project_root, project)
        // A lockfile-derived intersection has no lossless written form in
        // composer-semver. The selected exact version is a valid semantic
        // constraint and remains deterministic until the source inputs
        // change, which Phase 2's staleness check will detect separately.
        .unwrap_or_else(|| resolved.to_string())
}

pub(crate) fn php_constraint_input(project_root: &Path, project: &ProjectConfig) -> Option<String> {
    project
        .bougie
        .php
        .version
        .clone()
        .or_else(|| {
            project
                .composer
                .as_ref()
                .and_then(|composer| composer.require_php.clone())
        })
        .or_else(|| super::infer_php::infer_raw(project_root).and_then(|inferred| inferred.raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINUX: &str = "x86_64-unknown-linux-gnu";
    const MAC: &str = "aarch64-apple-darwin";
    const WINDOWS: &str = "x86_64-pc-windows-msvc";

    fn digest(seed: char) -> ArtifactDigest {
        ArtifactDigest {
            manifest_sha256: seed.to_string().repeat(64),
            blob_sha256: seed.to_string().repeat(64),
        }
    }

    fn lock(php: &str, targets: &[&str]) -> ToolchainLock {
        ToolchainLock {
            version: FORMAT_VERSION,
            snapshot: "20260101T000000Z".into(),
            php: PhpPin {
                constraint: "^8.3".into(),
                version: php.into(),
                flavor: "nts".into(),
            },
            extensions: BTreeMap::from([(
                "redis".into(),
                ExtensionPin {
                    constraint: "*".into(),
                    version: "6.2.0".into(),
                    origin: ExtensionOrigin::Declared,
                },
            )]),
            services: BTreeMap::new(),
            targets: targets
                .iter()
                .map(|target| {
                    (
                        (*target).to_owned(),
                        TargetArtifacts {
                            php: digest('a'),
                            extensions: BTreeMap::new(),
                            services: BTreeMap::new(),
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn upgrade_flags_release_the_named_pins() {
        assert_eq!(Upgrade::from_flags(false, vec![]), Upgrade::None);
        assert_eq!(Upgrade::from_flags(true, vec![]), Upgrade::All);
        let only = Upgrade::from_flags(false, vec!["php".into(), "ext-redis".into()]);
        assert!(only.allows("php") && only.allows("redis") && !only.allows("mysql"));
        assert!(Upgrade::All.allows("anything"));
        assert!(!Upgrade::None.allows("php"));

        let previous = lock("8.3.11", &[LINUX]);
        let prefer = Preferences::new(Some(&previous), &only);
        assert!(prefer.php().is_none());
        assert!(
            Preferences::new(Some(&previous), &Upgrade::None)
                .php()
                .is_some()
        );
        assert!(only.unknown_names(&previous).is_empty());
        assert_eq!(
            Upgrade::from_flags(false, vec!["mysql".into()]).unknown_names(&previous),
            ["mysql"]
        );
    }

    #[test]
    fn constraint_admits_kept_versions() {
        assert!(constraint_admits("*", "8.0.46"));
        assert!(constraint_admits("", "8.0.46"));
        assert!(constraint_admits("^8.0", "8.0.46"));
        assert!(constraint_admits("8.0.46", "8.0.46"));
        assert!(!constraint_admits("^8.4", "8.0.46"));
    }

    #[test]
    fn linux_only_baseline_extensions_are_skipped_per_target() {
        let baseline = ExtensionOrigin::Baseline;
        assert!(extension_expected_on("gettext", baseline, LINUX));
        assert!(!extension_expected_on("gettext", baseline, MAC));
        assert!(!extension_expected_on("gettext", baseline, WINDOWS));
        // Only the baseline set is skipped: a declared gettext is the
        // project's own requirement and must not silently disappear.
        assert!(extension_expected_on(
            "gettext",
            ExtensionOrigin::Declared,
            MAC
        ));
        assert!(extension_expected_on("redis", baseline, MAC));
    }

    #[test]
    fn services_are_never_expected_on_windows() {
        assert!(services_expected_on(LINUX));
        assert!(services_expected_on(MAC));
        assert!(!services_expected_on(WINDOWS));
    }

    #[test]
    fn relock_with_same_pins_keeps_snapshot_and_other_targets() {
        let previous = lock("8.3.12", &[LINUX, MAC]);
        let mut fresh = lock("8.3.12", &[LINUX]);
        fresh.snapshot = "20260202T000000Z".into();
        fresh.targets.get_mut(LINUX).unwrap().php = digest('b');

        merge_previous(&mut fresh, &previous);
        assert_eq!(fresh.snapshot, previous.snapshot);
        assert!(fresh.targets.contains_key(MAC));
        // The freshly recorded block for this machine wins.
        assert_eq!(fresh.targets[LINUX].php, digest('b'));
        assert!(describe_changes(Some(&previous), &fresh).is_empty());
    }

    #[test]
    fn relock_with_moved_pins_drops_other_targets_and_says_so() {
        let previous = lock("8.3.11", &[LINUX, MAC]);
        let mut fresh = lock("8.3.12", &[LINUX]);
        merge_previous(&mut fresh, &previous);
        assert!(!fresh.targets.contains_key(MAC));
        assert_eq!(
            describe_changes(Some(&previous), &fresh),
            [
                "php 8.3.11 (nts) -> 8.3.12 (nts)".to_owned(),
                format!("- digests for {MAC}"),
            ]
        );
    }

    #[test]
    fn describe_changes_lists_extension_moves() {
        let previous = lock("8.3.12", &[LINUX]);
        let mut fresh = previous.clone();
        fresh.extensions.get_mut("redis").unwrap().version = "6.2.1".into();
        fresh.extensions.insert(
            "apcu".into(),
            ExtensionPin {
                constraint: "*".into(),
                version: "5.1.24".into(),
                origin: ExtensionOrigin::Inferred,
            },
        );
        assert_eq!(
            describe_changes(Some(&previous), &fresh),
            ["extension redis 6.2.0 -> 6.2.1", "+ extension apcu 5.1.24"]
        );
        assert!(describe_changes(None, &fresh).is_empty());
    }
}
