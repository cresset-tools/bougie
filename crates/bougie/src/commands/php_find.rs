use bougie_cli::OutputFormat;
use bougie_errors::BougieError;
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use bougie_version::request::{parse_request, Flavor, Request, VersionLike};
use bougie_fs::store::{install_dir, list_installed};
use bougie_version::version::Version;
use eyre::{eyre, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

#[derive(Debug, Serialize)]
pub struct FindResult {
    pub schema_version: u32,
    pub path: PathBuf,
}

impl Render for FindResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "{}", self.path.display())
    }
}

pub fn run(
    format: OutputFormat,
        request_str: Option<&str>,
) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let php = match request_str {
        Some(s) => find_for_request(&paths, &parse_request(s)?)?,
        None => find_first_installed(&paths)?,
    };
    let result = FindResult { schema_version: 1, path: php };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn find_for_request(paths: &Paths, request: &Request) -> Result<PathBuf> {
    if let Request::Path(p) = request {
        let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        if canon.is_file() {
            return Ok(canon);
        }
        if canon.is_dir() {
            return Ok(canon.join("bin").join("php"));
        }
        return Err(eyre!("no such path: {}", p.display()));
    }
    let (pv, in_request_flavor) = match request {
        Request::VersionLike { spec, flavor } => match spec {
            VersionLike::Version(pv) if pv.is_exact() => (pv.pad(), *flavor),
            _ => return locate_best_match(paths, request),
        },
        Request::FullTag { version, flavor, .. } if version.is_exact() => {
            (version.pad(), *flavor)
        }
        _ => return locate_best_match(paths, request),
    };
    let flavor = in_request_flavor.unwrap_or(Flavor::Nts);
    let dir = install_dir(paths, pv, flavor);
    if !dir.exists() {
        return Err(BougieError::Resolution {
            kind: "php".into(),
            detail: format!(
                "no installed PHP at {} — run `bougie php install` first",
                dir.display()
            ),
        }
        .into());
    }
    Ok(dir.join("bin").join("php"))
}

fn locate_best_match(paths: &Paths, _request: &Request) -> Result<PathBuf> {
    // Without the index loaded, the best we can do for a non-exact
    // request is pick the highest installed PHP. Phase 8 widens this
    // to consult the cached section.
    find_first_installed(paths)
}

fn find_first_installed(paths: &Paths) -> Result<PathBuf> {
    let mut best: Option<(Version, String)> = None;
    for (v_str, flavor) in list_installed(paths)? {
        let Ok(v) = Version::from_str(&v_str) else {
            continue;
        };
        match &best {
            None => best = Some((v, flavor)),
            Some((bv, _)) if v > *bv => best = Some((v, flavor)),
            _ => {}
        }
    }
    let (v, flavor) = best.ok_or_else(|| BougieError::Resolution {
        kind: "php".into(),
        detail: "no PHP interpreter installed yet; run `bougie php install` first".into(),
    })?;
    let pv = bougie_version::version::PartialVersion {
        major: v.major,
        minor: Some(v.minor),
        patch: Some(v.patch),
    };
    let f = parse_flavor(&flavor).unwrap_or(Flavor::Nts);
    Ok(install_dir(paths, pv.pad(), f).join("bin").join("php"))
}

fn parse_flavor(s: &str) -> Option<Flavor> {
    Some(match s {
        "nts" => Flavor::Nts,
        "nts-debug" => Flavor::NtsDebug,
        "zts" => Flavor::Zts,
        "zts-debug" => Flavor::ZtsDebug,
        _ => return None,
    })
}
