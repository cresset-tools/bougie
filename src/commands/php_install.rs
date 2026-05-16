use crate::baseline::{parse_without, BaselineFilter};
use crate::cli::OutputFormat;
use crate::install::{
    install_baseline_into, install_php, preinstall_into, BaselineReport, InstalledPhp,
    PreinstallReport,
};
use crate::output::{emit, Render};
use crate::paths::Paths;
use crate::request::{parse_request, Flavor, Request, VersionLike};
use crate::resolve::ResolveOptions;
use crate::version::{Constraint, Op, PartialVersion};
use eyre::{eyre, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct InstallResult {
    pub schema_version: u32,
    pub installed: Vec<InstallEntry>,
}

#[derive(Debug, Serialize)]
pub struct InstallEntry {
    pub version: String,
    pub flavor: String,
    pub path: PathBuf,
    pub already_present: bool,
    /// Names of baseline extensions installed alongside this
    /// interpreter (CLI.md §3.5.1.1). Empty when `--bare`.
    pub baseline: Vec<String>,
    /// Per-name failure detail for baseline extensions that didn't
    /// install. The interpreter is still considered installed; the
    /// next `bougie sync` retries.
    pub baseline_failed: Vec<BaselineFailure>,
    /// Names of extensions pre-downloaded into the store but not
    /// enabled (currently: xdebug). Empty when `--bare`.
    pub preinstalled: Vec<String>,
    /// Per-name failure detail for preinstall, matching `baseline_failed`.
    pub preinstall_failed: Vec<BaselineFailure>,
}

#[derive(Debug, Serialize)]
pub struct BaselineFailure {
    pub name: String,
    pub reason: String,
}

impl Render for InstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for entry in &self.installed {
            let verb = if entry.already_present { "already" } else { "installed" };
            writeln!(
                w,
                "{verb} php {}-{} at {}",
                entry.version,
                entry.flavor,
                entry.path.display()
            )?;
            if !entry.baseline.is_empty() {
                writeln!(w, "  baseline: {}", entry.baseline.join(", "))?;
            }
            for failure in &entry.baseline_failed {
                writeln!(
                    w,
                    "  baseline failed: {} — {} (next `bougie sync` will retry)",
                    failure.name, failure.reason
                )?;
            }
            if !entry.preinstalled.is_empty() {
                writeln!(
                    w,
                    "  pre-downloaded (inactive): {}",
                    entry.preinstalled.join(", ")
                )?;
            }
            for failure in &entry.preinstall_failed {
                writeln!(
                    w,
                    "  preinstall failed: {} — {} (next `bougie sync` will retry)",
                    failure.name, failure.reason
                )?;
            }
        }
        Ok(())
    }
}

pub fn run(
    format: OutputFormat,
        request_strs: &[String],
    flavor_arg: Option<&str>,
    bare: bool,
    without: &[String],
) -> Result<ExitCode> {
    let flavor = match flavor_arg {
        Some(s) => Some(parse_flavor(s)?),
        None => None,
    };
    let baseline_filter = resolve_baseline_filter(bare, without)?;
    let paths = Paths::from_env()?;

    let requests: Vec<Request> = if request_strs.is_empty() {
        vec![default_latest_request()]
    } else {
        request_strs
            .iter()
            .map(|s| parse_request(s))
            .collect::<Result<_>>()?
    };

    let mut installed = Vec::with_capacity(requests.len());
    for request in &requests {
        let info: InstalledPhp =
            install_php(&paths, request, flavor, ResolveOptions::default())?;
        let php_minor = PartialVersion {
            major: info.version.major,
            minor: Some(info.version.minor),
            patch: None,
        };
        // Baseline install runs *after* install_php returns so the
        // global lock has been released — install_extension acquires
        // the same lock and nesting would deadlock. install.rs
        // documents this constraint on install_baseline_into.
        let report: BaselineReport = install_baseline_into(
            &paths,
            &info.install_path,
            php_minor,
            info.flavor,
            &baseline_filter,
            ResolveOptions::default(),
        );
        // Pre-download (without enabling) extensions like xdebug so
        // the first server-side debug request doesn't stall on a
        // download. Skipped under `--bare` so that flag still
        // produces a minimal install.
        let preinstall: PreinstallReport =
            if matches!(baseline_filter, BaselineFilter::None) {
                PreinstallReport::default()
            } else {
                preinstall_into(
                    &paths,
                    &info.install_path,
                    php_minor,
                    info.flavor,
                    ResolveOptions::default(),
                )
            };
        installed.push(InstallEntry {
            version: info.version.to_string(),
            flavor: info.flavor.to_string(),
            path: info.install_path,
            already_present: info.already_present,
            baseline: report.installed,
            baseline_failed: report
                .failed
                .into_iter()
                .map(|(name, reason)| BaselineFailure { name, reason })
                .collect(),
            preinstalled: preinstall.installed,
            preinstall_failed: preinstall
                .failed
                .into_iter()
                .map(|(name, reason)| BaselineFailure { name, reason })
                .collect(),
        });
    }

    let result = InstallResult { schema_version: 1, installed };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn resolve_baseline_filter(
    bare: bool,
    without: &[String],
) -> Result<BaselineFilter> {
    if bare && !without.is_empty() {
        // clap's conflicts_with usually catches this, but the resolver
        // is the second line of defense — callers pass slices directly
        // from tests.
        return Err(eyre!("--bare and --without are mutually exclusive"));
    }
    if bare {
        return Ok(BaselineFilter::None);
    }
    parse_without(without).map_err(|m| eyre!("{m}"))
}

/// `>= 0` — match anything (highest non-yanked overall). Used when the
/// user runs `bougie php install` with no argument.
fn default_latest_request() -> Request {
    Request::VersionLike {
        spec: VersionLike::Constraint(Constraint::Op(
            Op::Gte,
            PartialVersion { major: 0, minor: None, patch: None },
        )),
        flavor: None,
    }
}

fn parse_flavor(s: &str) -> Result<Flavor> {
    Ok(match s {
        "nts" => Flavor::Nts,
        "nts-debug" => Flavor::NtsDebug,
        "zts" => Flavor::Zts,
        "zts-debug" => Flavor::ZtsDebug,
        other => return Err(eyre!("unknown flavor: {other}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_filter_defaults_to_all() {
        match resolve_baseline_filter(false, &[]).unwrap() {
            BaselineFilter::All => {}
            other => panic!("expected All, got {other:?}"),
        }
    }

    #[test]
    fn bare_flag_disables_set() {
        match resolve_baseline_filter(true, &[]).unwrap() {
            BaselineFilter::None => {}
            other => panic!("expected None, got {other:?}"),
        }
    }

    #[test]
    fn without_excludes_named() {
        match resolve_baseline_filter(false, &["opcache".into(), "readline".into()])
            .unwrap()
        {
            BaselineFilter::Without(set) => {
                assert!(set.contains("opcache"));
                assert!(set.contains("readline"));
                assert!(!set.contains("calendar"));
            }
            other => panic!("expected Without(..), got {other:?}"),
        }
    }

    #[test]
    fn bare_and_without_conflict() {
        assert!(resolve_baseline_filter(true, &["opcache".into()]).is_err());
    }
}
