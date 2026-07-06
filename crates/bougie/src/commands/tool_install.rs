//! `bougie tool install <vendor/name>[@<constraint>] [--force]`.
//!
//! Bridges `bougie-tool::install::install` (which is composer-agnostic
//! by design) to the bougie binary's existing composer-resolve helper
//! at `composer_update::resolve_and_write_lock`. Other callback wiring
//! (PHP install, extension classify, extension install + conf.d) lives
//! in `super::tool_callbacks` so the four `tool_*` dispatchers share
//! the same code.

use bougie_cli::OutputFormat;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use bougie_tool::{install, request};
use eyre::Result;
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
    /// Launcher-cache path of the prefetched native binary, for tools
    /// declaring `extra.bougie.native-binary` (e.g. cresset/wick).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_binary: Option<PathBuf>,
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
        )?;
        if let Some(native) = &self.native_binary {
            writeln!(w, "native binary cached at {}", native.display())?;
        }
        Ok(())
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
        super::composer_update::resolve_and_write_lock(
            paths,
            project_root,
            bougie_composer_resolver::ResolutionStrategy::Highest,
        )
        .map(|_| ())
    };
    let php_installer = super::tool_callbacks::php_installer();
    let classifier = super::tool_callbacks::extension_classifier();
    let ext_installer = super::tool_callbacks::extension_installer();
    let tool_requires = super::tool_callbacks::tool_requires_fetcher();
    let php_baseline = super::tool_callbacks::baseline_ensurer();
    let native_fetcher = super::tool_callbacks::native_prefetcher();
    let ctx = install::InstallContext {
        paths: &paths,
        resolve_lock,
        php_installer: php_installer.as_ref(),
        classifier: classifier.as_ref(),
        ext_installer: ext_installer.as_ref(),
        tool_requires: tool_requires.as_ref(),
        php_baseline: php_baseline.as_ref(),
        native_fetcher: native_fetcher.as_ref(),
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
            native_binary: outcome.native_binary,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}
