use crate::cli::OutputFormat;
use crate::install::{install_php, InstalledPhp};
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
    pub version: String,
    pub flavor: String,
    pub path: PathBuf,
    pub already_present: bool,
}

impl Render for InstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let verb = if self.already_present { "already" } else { "installed" };
        writeln!(
            w,
            "{verb} php {}-{} at {}",
            self.version,
            self.flavor,
            self.path.display()
        )
    }
}

pub fn run(
    format: OutputFormat,
    field: Option<&str>,
    request_str: Option<&str>,
    flavor_arg: Option<&str>,
) -> Result<ExitCode> {
    let request = match request_str {
        Some(s) => parse_request(s)?,
        None => default_latest_request(),
    };
    let flavor = match flavor_arg {
        Some(s) => Some(parse_flavor(s)?),
        None => None,
    };
    let paths = Paths::from_env()?;
    let installed: InstalledPhp = install_php(&paths, &request, flavor, ResolveOptions::default())?;
    let result = InstallResult {
        schema_version: 1,
        version: installed.version.to_string(),
        flavor: installed.flavor.to_string(),
        path: installed.install_path,
        already_present: installed.already_present,
    };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
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
