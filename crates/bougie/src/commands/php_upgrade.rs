use bougie_cli::OutputFormat;
use bougie_fs::store::list_installed;
use bougie_installer::install::install_php;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use bougie_resolver::ResolveOptions;
use bougie_semver::Constraint;
use bougie_tool::receipt::{PhpUpgrade, refresh_php_pin};
use bougie_version::request::{Flavor, Request, VersionLike};
use bougie_version::version::PartialVersion;
use eyre::{Result, eyre};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

#[derive(Debug, Serialize)]
pub struct UpgradeResult {
    pub schema_version: u32,
    pub upgraded: Vec<UpgradeRow>,
    /// Tool receipts whose `php_resolved_path` + `php_version` were
    /// rewritten to point at the upgraded interpreter. Empty when no
    /// PHP actually moved or no tools were installed against the
    /// moved version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updated_tool_receipts: Vec<PathBuf>,
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
        if !self.updated_tool_receipts.is_empty() {
            writeln!(
                w,
                "refreshed {} tool receipt(s)",
                self.updated_tool_receipts.len()
            )?;
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat, minor_filter: Option<&str>) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let installed = list_installed(&paths)?;
    let mut upgraded = Vec::new();
    let mut tool_upgrades: Vec<PhpUpgrade> = Vec::new();

    for (version_str, flavor_str) in installed {
        let Ok(v) = bougie_version::version::Version::from_str(&version_str) else {
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
        let spec = VersionLike::Constraint(
            Constraint::parse(&format!(">={}.{}", v.major, v.minor))
                .map_err(|e| eyre!("constructing upgrade constraint: {e}"))?,
        );
        let request = Request::VersionLike { spec, flavor: Some(flavor) };
        let installed_now =
            install_php(&paths, &request, Some(flavor), ResolveOptions::default())?;
        if installed_now.version != v {
            upgraded.push(UpgradeRow {
                from: v.to_string(),
                to: installed_now.version.to_string(),
                flavor: flavor.to_string(),
            });
            tool_upgrades.push(PhpUpgrade {
                from_version: v.to_string(),
                to_version: installed_now.version.to_string(),
                flavor: flavor.to_string(),
                new_bin: installed_now.install_path.join("bin").join("php"),
            });
        }
    }

    let updated_tool_receipts = refresh_php_pin(&paths, &tool_upgrades)?;

    let result = UpgradeResult {
        schema_version: 1,
        upgraded,
        updated_tool_receipts,
    };
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
