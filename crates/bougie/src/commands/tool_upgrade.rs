//! `bougie tool upgrade <vendor/name>` / `--all` / `--reinstall`.

use bougie_cli::OutputFormat;
use bougie_installer::install::install_php;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use bougie_resolver::ResolveOptions;
use bougie_tool::classify::ExtensionClassifier;
use bougie_tool::install::{ExtInstaller, InstallContext};
use bougie_tool::resolve::{PhpChoice, PhpInstaller};
use bougie_tool::{request, upgrade};
use bougie_version::request::parse_request as parse_php_request;
use eyre::{Result, WrapErr};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ToolUpgradeResult {
    pub schema_version: u32,
    pub upgraded: Vec<UpgradeRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<UpgradeFailure>,
}

#[derive(Debug, Serialize)]
pub struct UpgradeRow {
    pub package: String,
    pub tool_dir: PathBuf,
    pub previous_php: String,
    pub current_php: String,
    pub installed_bins: Vec<PathBuf>,
    pub reinstalled: bool,
}

#[derive(Debug, Serialize)]
pub struct UpgradeFailure {
    pub package: String,
    pub error: String,
}

impl Render for ToolUpgradeResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.upgraded.is_empty() && self.failed.is_empty() {
            return writeln!(w, "no tools to upgrade");
        }
        for row in &self.upgraded {
            let php_note = if row.previous_php == row.current_php {
                String::new()
            } else {
                format!(" (php {} → {})", row.previous_php, row.current_php)
            };
            let mode = if row.reinstalled { " (reinstalled)" } else { "" };
            writeln!(w, "upgraded {}{php_note}{mode}", row.package)?;
        }
        for fail in &self.failed {
            writeln!(w, "FAILED {} — {}", fail.package, fail.error)?;
        }
        Ok(())
    }
}

pub fn run(
    format: OutputFormat,
    package: Option<&str>,
    all: bool,
    reinstall: bool,
) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let resolve_lock: &bougie_tool::install::LockResolver = &|paths, project_root| {
        super::composer_update::resolve_and_write_lock(paths, project_root).map(|_| ())
    };
    let php_installer: &PhpInstaller = &|paths, spec| {
        let request = parse_php_request(spec)
            .wrap_err_with(|| format!("parsing --php value `{spec}`"))?;
        let installed = install_php(paths, &request, None, ResolveOptions::default())
            .wrap_err_with(|| format!("installing PHP for --php {spec}"))?;
        Ok(PhpChoice {
            bin: installed.install_path.join("bin").join("php"),
            version: installed.version.to_string(),
            flavor: installed.flavor.as_str().to_string(),
        })
    };
    let classifier: &ExtensionClassifier = &|_name| Ok(false);
    let ext_installer: &ExtInstaller = &|_paths, name, _php| {
        Err(eyre::eyre!(
            "extension reinstall not wired yet; got `{name}`"
        ))
    };
    let ctx = InstallContext {
        paths: &paths,
        resolve_lock,
        php_installer,
        classifier,
        ext_installer,
    };

    let mut upgraded = Vec::new();
    let mut failed = Vec::new();
    let mut exit = ExitCode::SUCCESS;

    if all {
        for (pkg, result) in upgrade::upgrade_all(&ctx, reinstall)? {
            match result {
                Ok(o) => upgraded.push(row_from(o)),
                Err(e) => {
                    failed.push(UpgradeFailure {
                        package: pkg,
                        error: format!("{e:#}"),
                    });
                    exit = ExitCode::FAILURE;
                }
            }
        }
    } else {
        let Some(pkg) = package else {
            return Err(eyre::eyre!("internal: upgrade dispatched without a package or --all"));
        };
        let req = request::parse(pkg)?;
        let outcome = upgrade::upgrade_one(&ctx, &req.package(), reinstall)?;
        upgraded.push(row_from(outcome));
    }

    emit(
        format,
        &ToolUpgradeResult {
            schema_version: 1,
            upgraded,
            failed,
        },
    )?;
    Ok(exit)
}

fn row_from(o: bougie_tool::upgrade::UpgradeOutcome) -> UpgradeRow {
    UpgradeRow {
        package: o.package,
        tool_dir: o.tool_dir,
        previous_php: o.previous_php,
        current_php: o.current_php,
        installed_bins: o.installed_bins,
        reinstalled: o.reinstalled,
    }
}
