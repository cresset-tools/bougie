use crate::cli::OutputFormat;
use crate::errors::BougieError;
use crate::output::{emit, Render};
use crate::paths::Paths;
use crate::request::{parse_request, Flavor, Request, VersionLike};
use crate::store::install_dir;
use eyre::{eyre, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct UninstallResult {
    pub schema_version: u32,
    pub removed: PathBuf,
}

impl Render for UninstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "removed {}", self.removed.display())
    }
}

pub fn run(
    format: OutputFormat,
    field: Option<&str>,
    request_str: &str,
    flavor_arg: Option<&str>,
) -> Result<ExitCode> {
    let request = parse_request(request_str)?;
    let paths = Paths::from_env()?;
    let dest = locate_install(&paths, &request, flavor_arg)?;
    if !dest.exists() {
        return Err(BougieError::Resolution(format!("no install at {}", dest.display())).into());
    }
    std::fs::remove_dir_all(&dest)
        .map_err(|e| BougieError::Filesystem(format!("removing {}: {e}", dest.display())))?;
    let result = UninstallResult { schema_version: 1, removed: dest };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn locate_install(paths: &Paths, request: &Request, flavor_arg: Option<&str>) -> Result<PathBuf> {
    if let Request::Path(p) = request {
        let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        if !canon.starts_with(paths.installs()) {
            return Err(eyre!(
                "path {} is not under {}",
                p.display(),
                paths.installs().display()
            ));
        }
        return Ok(canon);
    }
    let (version_pv, in_request_flavor) = match request {
        Request::VersionLike { spec, flavor } => match spec {
            VersionLike::Version(pv) if pv.is_exact() => (pv.pad(), *flavor),
            _ => {
                return Err(BougieError::Resolution(
                    "uninstall requires an exact version".into(),
                )
                .into())
            }
        },
        Request::FullTag { version, flavor, .. } if version.is_exact() => {
            (version.pad(), *flavor)
        }
        _ => {
            return Err(BougieError::Resolution(
                "uninstall requires an exact (version, flavor) target".into(),
            )
            .into())
        }
    };
    let flavor = resolve_flavor(in_request_flavor, flavor_arg)?;
    Ok(install_dir(paths, version_pv, flavor))
}

fn resolve_flavor(in_request: Option<Flavor>, flag: Option<&str>) -> Result<Flavor> {
    let parsed = match flag {
        Some(s) => Some(parse_flavor(s)?),
        None => None,
    };
    match (in_request, parsed) {
        (Some(a), Some(b)) if a != b => Err(eyre!("flavor mismatch")),
        (Some(f), _) | (None, Some(f)) => Ok(f),
        (None, None) => Ok(Flavor::Nts),
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
