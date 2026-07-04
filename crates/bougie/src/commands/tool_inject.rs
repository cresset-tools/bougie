//! `bougie tool inject <vendor/name> --with <extra>...`.

use bougie_cli::OutputFormat;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use bougie_tool::install::InstallContext;
use bougie_tool::{inject, request};
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ToolInjectResult {
    pub schema_version: u32,
    pub package: String,
    pub tool_dir: PathBuf,
    pub added_composer: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_extensions: Vec<String>,
}

impl Render for ToolInjectResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let mut bits = self.added_composer.clone();
        for ext in &self.added_extensions {
            bits.push(format!("ext-{ext}"));
        }
        writeln!(w, "injected into {}: {}", self.package, bits.join(", "))
    }
}

pub fn run(format: OutputFormat, package: &str, with: &[String]) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let req = request::parse(package)?;
    let resolve_lock: &bougie_tool::install::LockResolver = &|paths, project_root| {
        super::composer_update::resolve_and_write_lock(paths, project_root, bougie_composer_resolver::ResolutionStrategy::Highest).map(|_| ())
    };
    let php_installer = super::tool_callbacks::php_installer();
    let classifier = super::tool_callbacks::extension_classifier();
    let ext_installer = super::tool_callbacks::extension_installer();
    let tool_requires = super::tool_callbacks::tool_requires_fetcher();
    let php_baseline = super::tool_callbacks::baseline_ensurer();
    let ctx = InstallContext {
        paths: &paths,
        resolve_lock,
        php_installer: php_installer.as_ref(),
        classifier: classifier.as_ref(),
        ext_installer: ext_installer.as_ref(),
        tool_requires: tool_requires.as_ref(),
        php_baseline: php_baseline.as_ref(),
    };
    let outcome = inject::inject(&ctx, &req.package(), with)?;
    emit(
        format,
        &ToolInjectResult {
            schema_version: 1,
            package: outcome.package,
            tool_dir: outcome.tool_dir,
            added_composer: outcome.added_composer,
            added_extensions: outcome.added_extensions,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}
