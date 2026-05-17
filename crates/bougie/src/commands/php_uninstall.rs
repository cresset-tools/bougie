use bougie_cli::OutputFormat;
use bougie_errors::BougieError;
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use bougie_version::request::{parse_request, Flavor, Request, VersionLike};
use bougie_fs::store::install_dir;
use eyre::{eyre, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct UninstallResult {
    pub schema_version: u32,
    pub removed: Vec<RemovedEntry>,
}

#[derive(Debug, Serialize)]
pub struct RemovedEntry {
    pub path: PathBuf,
}

impl Render for UninstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for entry in &self.removed {
            writeln!(w, "removed {}", entry.path.display())?;
        }
        Ok(())
    }
}

pub fn run(
    format: OutputFormat,
        request_strs: &[String],
    flavor_arg: Option<&str>,
) -> Result<ExitCode> {
    let paths = Paths::from_env()?;

    let mut targets = Vec::with_capacity(request_strs.len());
    for s in request_strs {
        let request = parse_request(s)?;
        let dest = locate_install(&paths, &request, flavor_arg)?;
        if !dest.exists() {
            return Err(BougieError::Resolution {
                kind: "uninstall".into(),
                detail: format!("no install directory at {}", dest.display()),
            }
            .into());
        }
        targets.push(dest);
    }

    let mut removed = Vec::with_capacity(targets.len());
    for dest in targets {
        std::fs::remove_dir_all(&dest).map_err(|e| BougieError::Filesystem {
            operation: format!("removing {}", dest.display()),
            detail: e.to_string(),
        })?;
        removed.push(RemovedEntry { path: dest });
    }

    let result = UninstallResult { schema_version: 1, removed };
    emit(format, &result)?;
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
                return Err(BougieError::Resolution {
                    kind: "uninstall".into(),
                    detail: "uninstall requires an exact version (e.g. `8.3.12`), not a constraint".into(),
                }
                .into())
            }
        },
        Request::FullTag { version, flavor, .. } if version.is_exact() => {
            (version.pad(), *flavor)
        }
        _ => {
            return Err(BougieError::Resolution {
                kind: "uninstall".into(),
                detail: "uninstall requires an exact (version, flavor) target — pass e.g. `bougie php uninstall 8.3.12 --flavor nts`".into(),
            }
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
