//! `bougie tool install <vendor/name>[@<constraint>] [--force]`.
//!
//! Bridges `bougie-tool::install::install` (which is composer-agnostic
//! by design) to the bougie binary's existing composer-resolve helper
//! at `composer_update::resolve_and_write_lock`. Keeping the bougie
//! crate's resolver glue out of `bougie-tool` avoids a circular crate
//! dep — `bougie-composer-resolver` already lives below us in the
//! workspace graph for the install step; only the *lock generation*
//! glue lives up here.

use bougie_cli::OutputFormat;
use bougie_installer::install::install_php;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use bougie_resolver::ResolveOptions;
use bougie_tool::classify::ExtensionClassifier;
use bougie_tool::install::ExtInstaller;
use bougie_tool::resolve::{PhpChoice, PhpInstaller};
use bougie_tool::{install, request};
use bougie_version::request::parse_request as parse_php_request;
use eyre::{Result, WrapErr};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ToolInstallResult {
    pub schema_version: u32,
    pub package: String,
    pub php_version: String,
    pub tool_dir: PathBuf,
    pub installed_bins: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub installed_extensions: Vec<String>,
}

impl Render for ToolInstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let bins = self
            .installed_bins
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let exts = if self.installed_extensions.is_empty() {
            String::new()
        } else {
            format!(" (+ exts: {})", self.installed_extensions.join(", "))
        };
        writeln!(
            w,
            "installed {} (php {}){exts} → {bins}",
            self.package, self.php_version
        )
    }
}

pub fn run(
    format: OutputFormat,
    package: &str,
    php_spec: Option<&str>,
    with: &[String],
    force: bool,
) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let req = request::parse(package)?;
    let resolve_lock: &install::LockResolver = &|paths, project_root| {
        super::composer_update::resolve_and_write_lock(paths, project_root)
            .map(|_| ())
    };
    let php_installer: &PhpInstaller = &|paths, spec| {
        let request = parse_php_request(spec)
            .wrap_err_with(|| format!("parsing --php value `{spec}`"))?;
        let installed = install_php(paths, &request, None, ResolveOptions::default())
            .wrap_err_with(|| format!("installing PHP for --php {spec}"))?;
        let version = installed.version.to_string();
        let flavor = installed.flavor.as_str().to_string();
        Ok(PhpChoice {
            bin: installed.install_path.join("bin").join("php"),
            version,
            flavor,
        })
    };
    // Phase 2 PR4 ships composer-package `--with` only. The classifier
    // refuses bare names so the user sees the recovery hint
    // (`bougie ext add` then `bougie tool inject` once that wiring
    // lands) rather than the call hitting an unwired ext-installer.
    let classifier: &ExtensionClassifier = &|_name| Ok(false);
    let ext_installer: &ExtInstaller = &|_paths, name, _php| {
        Err(eyre::eyre!(
            "PHP extension `--with {name}` is not wired yet; \
             use `vendor/name` for composer packages instead"
        ))
    };
    let ctx = install::InstallContext {
        paths: &paths,
        resolve_lock,
        php_installer,
        classifier,
        ext_installer,
    };
    let outcome = install::install(&ctx, &req, php_spec, with, force)?;
    emit(
        format,
        &ToolInstallResult {
            schema_version: 1,
            package: outcome.package,
            php_version: outcome.php_version,
            tool_dir: outcome.tool_dir,
            installed_bins: outcome.installed_bins,
            installed_extensions: outcome.installed_extensions,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}
