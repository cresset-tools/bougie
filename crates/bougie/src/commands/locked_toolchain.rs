//! Load and semantically validate `bougie.lock` for install-time use.

use bougie_config::ProjectConfig;
use bougie_lock::ToolchainLock;
use bougie_paths::Paths;
use bougie_platform::target::Triple;
use bougie_version::request::{Flavor, VersionLike};
use eyre::Result;
use std::collections::BTreeMap;
use std::path::Path;

pub fn load_fresh(
    project_root: &Path,
    paths: &Paths,
    project: &ProjectConfig,
    php_spec: &VersionLike,
    flavor: Flavor,
    offline: bool,
) -> Result<Option<ToolchainLock>> {
    let Some(mut lock) = ToolchainLock::read(project_root)? else {
        return Ok(None);
    };
    let mut drift = Vec::new();

    if let Some(constraint) = super::platform_lock::php_constraint_input(project_root, project)
        && constraint != lock.php.constraint
    {
        drift.push(format!(
            "php constraint changed from {:?} to {:?}",
            lock.php.constraint, constraint
        ));
    }
    if lock.php.flavor != flavor.to_string() {
        drift.push(format!(
            "php flavor changed from {:?} to {:?}",
            lock.php.flavor,
            flavor.to_string()
        ));
    }
    match lock.php.version.parse::<bougie_version::version::Version>() {
        Ok(version) if bougie_version::matches::version_satisfies(&version, php_spec) => {}
        Ok(version) => drift.push(format!(
            "locked php {version} no longer satisfies the project constraint"
        )),
        Err(error) => drift.push(format!("locked php version is invalid: {error}")),
    }

    let current_extensions = super::platform_lock::extension_inputs(
        project_root,
        project,
        lock.php
            .version
            .parse::<bougie_version::version::Version>()
            .map(|version| bougie_version::version::PartialVersion {
                major: version.major,
                minor: Some(version.minor),
                patch: None,
            })
            .unwrap_or(bougie_version::version::PartialVersion {
                major: 0,
                minor: Some(0),
                patch: None,
            }),
    );
    for (name, input) in &current_extensions {
        match lock.extensions.get(name) {
            None => drift.push(format!("extension {name} was added")),
            Some(pin) if pin.constraint != input.constraint => drift.push(format!(
                "extension {name} constraint changed from {:?} to {:?}",
                pin.constraint, input.constraint
            )),
            Some(pin) if pin.origin != input.origin => {
                drift.push(format!("extension {name} origin changed"));
            }
            Some(pin) => match (
                composer_semver::Constraint::parse(&pin.constraint),
                composer_semver::Version::parse(&pin.version),
            ) {
                (Ok(constraint), Ok(version)) if constraint.matches(&version) => {}
                _ => drift.push(format!(
                    "locked extension {name} {} no longer satisfies {:?}",
                    pin.version, pin.constraint
                )),
            },
        }
    }
    for name in lock.extensions.keys() {
        if !current_extensions.contains_key(name) {
            drift.push(format!("extension {name} was removed"));
        }
    }

    let current_services: BTreeMap<&str, &str> = project
        .bougie
        .services
        .iter()
        .map(|(name, pin)| (name.as_str(), pin.version().unwrap_or("*")))
        .collect();
    for (name, constraint) in &current_services {
        match lock.services.get(*name) {
            None => drift.push(format!("service {name} was added")),
            Some(pin) if pin.constraint != *constraint => drift.push(format!(
                "service {name} constraint changed from {:?} to {:?}",
                pin.constraint, constraint
            )),
            Some(_) => {}
        }
    }
    for name in lock.services.keys() {
        if !current_services.contains_key(name.as_str()) {
            drift.push(format!("service {name} was removed"));
        }
    }

    let target = Triple::detect()?.to_string();
    match lock.targets.get(&target) {
        None if drift.is_empty() && !offline => {
            let (resolved_target, artifacts) =
                super::platform_lock::resolve_locked_target(&lock, paths)?;
            lock.targets.insert(resolved_target, artifacts);
            lock.write(project_root)?;
            eprintln!("added current target {target} to bougie.lock");
        }
        None if offline => {
            return Err(eyre::eyre!(
                "bougie.lock does not cover current target {target} and --offline prevents adding it; run `bougie sync` online once and commit the updated lock"
            ));
        }
        None => drift.push(format!("current target {target} is not covered")),
        Some(artifacts) => {
            for name in lock.extensions.keys() {
                if !artifacts.extensions.contains_key(name) {
                    drift.push(format!("current target has no digest for extension {name}"));
                }
            }
        }
    }

    if drift.is_empty() {
        Ok(Some(lock))
    } else if offline {
        Err(eyre::eyre!(
            "bougie.lock is stale and --offline prevents floating resolution; run `bougie lock --with-platform` online"
        ))
    } else {
        for detail in drift {
            eprintln!("warning: bougie.lock is stale: {detail}");
        }
        eprintln!("warning: run `bougie lock --with-platform` to refresh it");
        Ok(None)
    }
}
