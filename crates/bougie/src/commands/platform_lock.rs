//! Resolve the current project's toolchain into a committed `bougie.lock`.

use bougie_config::{ProjectConfig, load_project};
#[cfg(unix)]
use bougie_daemon::daemon::catalog;
use bougie_index::fetch::{fetch_manifest, fetch_root, fetch_section};
use bougie_index::{Root, Section, build_verifier, host_to_dirname};
use bougie_installer::baseline::{self, BASELINE_EXTENSIONS};
use bougie_installer::install::DEFAULT_INDEX_URL;
use bougie_lock::{
    ArtifactDigest, ExtensionOrigin, ExtensionPin, FORMAT_VERSION, PhpPin, ServicePin,
    TargetArtifacts, ToolchainLock,
};
use bougie_paths::Paths;
use bougie_platform::target::Triple;
use bougie_resolver::{ResolveOptions, resolve_extension_constraint, resolve_php};
use bougie_version::version::PartialVersion;
use eyre::{Result, WrapErr, eyre};
use std::collections::BTreeMap;
use std::path::Path;

pub fn resolve(project_root: &Path, paths: &Paths) -> Result<ToolchainLock> {
    let project = load_project(project_root)?;
    let (php_spec, flavor) = super::sync::project_php_inputs(project_root, &project)?;
    let target = Triple::detect()?.to_string();
    let host = std::env::var("BOUGIE_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.into());
    let client = bougie_fetch::default_client()?;
    let cache_root = paths.cache_index(&host_to_dirname(&host));
    let fetched = fetch_root(&client, &host, &cache_root, build_verifier)?;

    let php_section = fetch_named_section(
        &client,
        &fetched.root,
        &host,
        &cache_root,
        &target,
        "interpreter/php",
    )?;
    let selected = resolve_php(&php_section, &php_spec, flavor, ResolveOptions::default())?;
    let php_version = selected.version;
    let php_manifest_ref = selected.artifact.manifest.clone();
    let php_manifest = fetch_manifest(
        &client,
        &host,
        &cache_root,
        &php_manifest_ref.path,
        &php_manifest_ref.sha256,
    )?;
    php_manifest.validate()?;

    let php_minor = PartialVersion {
        major: php_version.major,
        minor: Some(php_version.minor),
        patch: None,
    };
    let mut extension_pins = BTreeMap::new();
    let mut extension_digests = BTreeMap::new();
    for (name, input) in extension_inputs(project_root, &project, php_minor) {
        let section_name = format!("extension/{name}");
        let section = fetch_named_section(
            &client,
            &fetched.root,
            &host,
            &cache_root,
            &target,
            &section_name,
        )?;
        let constraint =
            composer_semver::Constraint::parse(&input.constraint).map_err(|error| {
                eyre!(
                    "extension {name} constraint {:?}: {error}",
                    input.constraint
                )
            })?;
        let selected = resolve_extension_constraint(
            &section,
            php_minor,
            flavor,
            &constraint,
            ResolveOptions::default(),
        )
        .wrap_err_with(|| format!("resolving extension {name}"))?;
        let version = selected.version.to_string();
        let manifest_ref = selected.artifact.manifest.clone();
        let manifest = fetch_manifest(
            &client,
            &host,
            &cache_root,
            &manifest_ref.path,
            &manifest_ref.sha256,
        )?;
        manifest.validate()?;
        extension_digests.insert(
            name.clone(),
            ArtifactDigest {
                manifest_sha256: manifest_ref.sha256,
                blob_sha256: manifest.blob.sha256,
            },
        );
        extension_pins.insert(
            name,
            ExtensionPin {
                constraint: input.constraint,
                version,
                origin: input.origin,
            },
        );
    }

    #[cfg(unix)]
    let mut service_pins: BTreeMap<String, ServicePin> = BTreeMap::new();
    #[cfg(not(unix))]
    let service_pins: BTreeMap<String, ServicePin> = BTreeMap::new();
    #[cfg(unix)]
    let mut service_digests: BTreeMap<String, ArtifactDigest> = BTreeMap::new();
    #[cfg(not(unix))]
    let service_digests: BTreeMap<String, ArtifactDigest> = BTreeMap::new();
    #[cfg(unix)]
    for (name, pin) in &project.bougie.services {
        let entry = catalog::find(name)
            .ok_or_else(|| eyre!("service `{name}` is not in this bougie service catalog"))?;
        let version = super::service::up::resolve_service_version(name, pin)?;
        service_pins.insert(
            name.clone(),
            ServicePin {
                constraint: pin.version().unwrap_or("*").to_owned(),
                version: version.clone(),
            },
        );
        if entry.tarball.is_empty() {
            continue;
        }
        let section_name = format!("tool/{name}");
        let section = fetch_named_section(
            &client,
            &fetched.root,
            &host,
            &cache_root,
            &target,
            &section_name,
        )?;
        let artifact = section
            .artifacts
            .iter()
            .find(|artifact| artifact.version == version && !artifact.yanked)
            .ok_or_else(|| {
                eyre!(
                    "the index at {host} does not publish service `{name}` at {version} for {target}"
                )
            })?;
        let manifest_ref = artifact.manifest.clone();
        let manifest = fetch_manifest(
            &client,
            &host,
            &cache_root,
            &manifest_ref.path,
            &manifest_ref.sha256,
        )?;
        manifest.validate()?;
        service_digests.insert(
            name.clone(),
            ArtifactDigest {
                manifest_sha256: manifest_ref.sha256,
                blob_sha256: manifest.blob.sha256,
            },
        );
    }

    Ok(ToolchainLock {
        version: FORMAT_VERSION,
        snapshot: fetched.root.version,
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
                php: ArtifactDigest {
                    manifest_sha256: php_manifest_ref.sha256,
                    blob_sha256: php_manifest.blob.sha256,
                },
                extensions: extension_digests,
                services: service_digests,
            },
        )]),
    })
}

pub(crate) fn resolve_locked_target(
    lock: &ToolchainLock,
    paths: &Paths,
) -> Result<(String, TargetArtifacts)> {
    let target = Triple::detect()?.to_string();
    let host = std::env::var("BOUGIE_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.into());
    let client = bougie_fetch::default_client()?;
    let cache_root = paths.cache_index(&host_to_dirname(&host));
    let fetched = fetch_root(&client, &host, &cache_root, build_verifier)?;
    let flavor = match lock.php.flavor.as_str() {
        "nts" => bougie_version::request::Flavor::Nts,
        "nts-debug" => bougie_version::request::Flavor::NtsDebug,
        "zts" => bougie_version::request::Flavor::Zts,
        "zts-debug" => bougie_version::request::Flavor::ZtsDebug,
        other => return Err(eyre!("bougie.lock has unknown PHP flavor {other:?}")),
    };
    let php_version = lock
        .php
        .version
        .parse::<bougie_version::version::Version>()?;
    let php_spec = bougie_version::request::VersionLike::Version(PartialVersion {
        major: php_version.major,
        minor: Some(php_version.minor),
        patch: Some(php_version.patch),
    });
    let php_section = fetch_named_section(
        &client,
        &fetched.root,
        &host,
        &cache_root,
        &target,
        "interpreter/php",
    )?;
    let selected = resolve_php(
        &php_section,
        &php_spec,
        flavor,
        ResolveOptions { allow_yanked: true },
    )?;
    let php_ref = selected.artifact.manifest.clone();
    let php_manifest = fetch_manifest(&client, &host, &cache_root, &php_ref.path, &php_ref.sha256)?;
    php_manifest.validate()?;

    let php_minor = PartialVersion {
        major: php_version.major,
        minor: Some(php_version.minor),
        patch: None,
    };
    let mut extensions = BTreeMap::new();
    for (name, pin) in &lock.extensions {
        let section = fetch_named_section(
            &client,
            &fetched.root,
            &host,
            &cache_root,
            &target,
            &format!("extension/{name}"),
        )?;
        let selected = bougie_resolver::resolve_extension(
            &section,
            php_minor,
            flavor,
            Some(&pin.version),
            ResolveOptions { allow_yanked: true },
        )?;
        let manifest_ref = selected.artifact.manifest.clone();
        let manifest = fetch_manifest(
            &client,
            &host,
            &cache_root,
            &manifest_ref.path,
            &manifest_ref.sha256,
        )?;
        manifest.validate()?;
        extensions.insert(
            name.clone(),
            ArtifactDigest {
                manifest_sha256: manifest_ref.sha256,
                blob_sha256: manifest.blob.sha256,
            },
        );
    }

    #[cfg(unix)]
    let mut services = BTreeMap::new();
    #[cfg(not(unix))]
    let services = BTreeMap::new();
    #[cfg(unix)]
    for (name, pin) in &lock.services {
        if catalog::find(name).is_some_and(|entry| entry.tarball.is_empty()) {
            continue;
        }
        let section = fetch_named_section(
            &client,
            &fetched.root,
            &host,
            &cache_root,
            &target,
            &format!("tool/{name}"),
        )?;
        let artifact = section
            .artifacts
            .iter()
            .find(|artifact| artifact.version == pin.version)
            .ok_or_else(|| eyre!("service {name} {} is unavailable for {target}", pin.version))?;
        let manifest_ref = artifact.manifest.clone();
        let manifest = fetch_manifest(
            &client,
            &host,
            &cache_root,
            &manifest_ref.path,
            &manifest_ref.sha256,
        )?;
        manifest.validate()?;
        services.insert(
            name.clone(),
            ArtifactDigest {
                manifest_sha256: manifest_ref.sha256,
                blob_sha256: manifest.blob.sha256,
            },
        );
    }

    Ok((
        target,
        TargetArtifacts {
            php: ArtifactDigest {
                manifest_sha256: php_ref.sha256,
                blob_sha256: php_manifest.blob.sha256,
            },
            extensions,
            services,
        },
    ))
}

fn fetch_named_section(
    client: &reqwest::blocking::Client,
    root: &Root,
    host: &str,
    cache_root: &Path,
    target: &str,
    name: &str,
) -> Result<Section> {
    let target_entry = root.targets.get(target).ok_or_else(|| {
        eyre!(
            "the index at {host} does not serve {target}; available targets: {}",
            root.targets.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    })?;
    let section_ref = target_entry.sections.get(name).ok_or_else(|| {
        eyre!("the index at {host} has no `{name}` section under target {target}")
    })?;
    fetch_section(
        client,
        host,
        cache_root,
        &root.version,
        target,
        name,
        &section_ref.sha256,
    )
}

#[derive(Debug)]
pub(crate) struct ExtensionInput {
    pub constraint: String,
    pub origin: ExtensionOrigin,
}

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
            && !baseline::skip_for_platform(name)
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
