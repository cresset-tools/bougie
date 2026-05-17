use crate::cli::OutputFormat;
use crate::install::install_php;
use crate::output::{emit, Render};
use crate::paths::Paths;
use crate::request::{Flavor, Request, VersionLike};
use crate::resolve::ResolveOptions;
use crate::store::list_installed;
use crate::version::{Constraint, Op, PartialVersion};
use eyre::{eyre, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::process::ExitCode;
use std::str::FromStr;

#[derive(Debug, Serialize)]
pub struct UpgradeResult {
    pub schema_version: u32,
    pub upgraded: Vec<UpgradeRow>,
}

#[derive(Debug, Serialize)]
pub struct UpgradeRow {
    pub from: String,
    pub to: String,
    pub flavor: String,
}

impl Render for UpgradeResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.upgraded.is_empty() {
            writeln!(w, "no installed interpreters needed an upgrade")?;
            return Ok(());
        }
        for row in &self.upgraded {
            writeln!(w, "  {} ({}) → {}", row.from, row.flavor, row.to)?;
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat, minor_filter: Option<&str>) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let installed = list_installed(&paths)?;
    let mut upgraded = Vec::new();

    for (version_str, flavor_str) in installed {
        let Ok(v) = crate::version::Version::from_str(&version_str) else {
            continue;
        };
        let flavor = parse_flavor(&flavor_str).ok_or_else(|| eyre!("unknown flavor: {flavor_str}"))?;
        if let Some(want) = minor_filter {
            let want_pv = PartialVersion::parse(want)?;
            if want_pv.major != v.major || want_pv.minor.is_some_and(|m| m != v.minor) {
                continue;
            }
        }
        // Resolve the highest patch within (major, minor).
        let spec = VersionLike::Constraint(Constraint::Op(
            Op::Gte,
            PartialVersion { major: v.major, minor: Some(v.minor), patch: None },
        ));
        let request = Request::VersionLike { spec, flavor: Some(flavor) };
        let installed_now =
            install_php(&paths, &request, Some(flavor), ResolveOptions::default())?;
        if installed_now.version != v {
            upgraded.push(UpgradeRow {
                from: v.to_string(),
                to: installed_now.version.to_string(),
                flavor: flavor.to_string(),
            });
        }
    }

    let result = UpgradeResult { schema_version: 1, upgraded };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
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
