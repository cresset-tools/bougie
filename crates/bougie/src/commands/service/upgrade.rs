//! `bougie service upgrade <name> [--to <version>]`.
//!
//! Moves this project to a newer release line of a service and takes
//! its data along. An unpinned project stays on the line it already has
//! data on (see [`super::up::resolve_service_version_keeping_line`]), so
//! crossing to another line is something the user asks for, here.
//!
//! The daemon stops the old instance, copies its data directory into
//! the new instance and moves this project's tenant row
//! (`service.migrate`). The project's pin is then set to the new major
//! (`opensearch = "3"`), which also takes it past a `bougie.lock`
//! entry for the old version. Going back is pinning the old major
//! again: its instance's data is the untouched copy.

use super::client;
use super::config_mut::{choose_config_target, locate_project_root, set_service_version};
use super::up::resolve_service_version;
use bougie_cli::OutputFormat;
use bougie_config::{ServicePin, load_project};
use bougie_daemon::daemon::{catalog, tenants};
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use eyre::{Result, eyre};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ServiceUpgradeResult {
    pub schema_version: u32,
    pub service: String,
    pub from: String,
    pub to: String,
    /// The pin written to the project's config.
    pub pin: String,
    /// The pin that returns the project to the old line.
    pub rollback_pin: String,
    /// The project has a `bougie.lock` that records this service.
    pub relock: bool,
}

#[derive(Debug, Deserialize)]
struct DaemonReply {}

impl Render for ServiceUpgradeResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let Self {
            service,
            from,
            to,
            pin,
            rollback_pin,
            ..
        } = self;
        writeln!(w, "upgraded {service} {from} → {to}, data copied")?;
        writeln!(w, "pinned {service} = \"{pin}\"")?;
        writeln!(w, "next: `bougie up {service}` to start it")?;
        if self.relock {
            writeln!(
                w,
                "then: `bougie lock --with-platform` so bougie.lock records {to}"
            )?;
        }
        writeln!(
            w,
            "roll back: pin {service} = \"{rollback_pin}\"; the {from} data is as it was before the upgrade"
        )
    }
}

pub fn run(format: OutputFormat, name: &str, to: Option<&str>) -> Result<ExitCode> {
    let project_root = locate_project_root()?;
    let project = load_project(&project_root)?;
    if !project.bougie.services.contains_key(name) {
        return Err(eyre!("service `{name}` isn't declared in this project"));
    }
    let entry =
        catalog::find(name).ok_or_else(|| eyre!("service `{name}` is not in bougie's catalog"))?;
    if !bougie_daemon::daemon::migrate::supported(entry) {
        return Err(eyre!(
            "`bougie service upgrade` does not support {name} yet; change its pin instead \
             (the new version starts with an empty instance)"
        ));
    }

    let paths = Paths::from_env()?;
    let from = tenants::project_instance_versions(&paths, name, &project_root)
        .into_iter()
        .next()
        .ok_or_else(|| {
            eyre!(
                "this project has no {name} data yet, so there is nothing to carry over; \
                 pin the version you want instead"
            )
        })?;
    let target = match to {
        Some(constraint) => resolve_service_version(name, &ServicePin::Version(constraint.into()))?,
        None => newest(entry).to_owned(),
    };
    check_upgrade(name, &from, &target)?;

    let args = json!({
        "project": project_root,
        "service": name,
        "from": from,
        "to": target,
    });
    let _: DaemonReply = client::call(&paths, "service.migrate", args)?;

    let pin = major(&target).to_owned();
    set_service_version(&choose_config_target(&project_root)?, name, &pin)?;
    let relock = bougie_lock::ToolchainLock::read(&project_root)?
        .is_some_and(|lock| lock.services.contains_key(name));

    emit(
        format,
        &ServiceUpgradeResult {
            schema_version: 1,
            service: name.to_owned(),
            rollback_pin: major(&from).to_owned(),
            from,
            to: target,
            pin,
            relock,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}

fn newest(entry: &catalog::CatalogEntry) -> &'static str {
    std::iter::once(entry.version)
        .chain(entry.versions.iter().copied())
        .max_by(|a, b| catalog::cmp_versions(a, b))
        .unwrap_or(entry.version)
}

fn major(version: &str) -> &str {
    version.split('.').next().unwrap_or(version)
}

/// An upgrade goes forward, to another release line, and at most one
/// major at a time: `OpenSearch` reads indices from the previous major
/// only, and never data a newer version has written.
fn check_upgrade(name: &str, from: &str, to: &str) -> Result<()> {
    let (Some(f), Some(t)) = (catalog::release_line(from), catalog::release_line(to)) else {
        return Err(eyre!("{name}: can't compare versions {from} and {to}"));
    };
    if f == t {
        return Err(eyre!(
            "{name} is already on {from}; patch versions of the same line are taken over by \
             `bougie up` without an upgrade"
        ));
    }
    if t < f {
        return Err(eyre!(
            "{name}: {to} is older than {from}, and data written by a newer version can't be \
             opened by an older one. To go back to an earlier line, pin it: its instance still \
             has the data from before the upgrade"
        ));
    }
    if t.0 > f.0 + 1 {
        return Err(eyre!(
            "{name}: {from} → {to} skips a major version; upgrade one major at a time"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrades_go_forward_one_major_at_a_time() {
        assert!(check_upgrade("opensearch", "2.19.5", "3.10.1").is_ok());
        assert!(check_upgrade("opensearch", "2.18.0", "2.19.5").is_ok());
        assert!(check_upgrade("opensearch", "2.19.5", "2.19.6").is_err());
        assert!(check_upgrade("opensearch", "3.10.1", "2.19.5").is_err());
        assert!(check_upgrade("opensearch", "1.3.20", "3.10.1").is_err());
    }

    #[test]
    fn default_target_is_the_newest_shipped_version() {
        let os = catalog::find("opensearch").unwrap();
        assert_eq!(newest(os), "3.10.1");
        assert_eq!(major("3.10.1"), "3");
    }
}
