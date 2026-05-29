//! `bougie tool uninject <vendor/name> --with <extra>...`.

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
pub struct ToolUninjectResult {
    pub schema_version: u32,
    pub package: String,
    pub tool_dir: PathBuf,
    pub removed_composer: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_extensions: Vec<String>,
}

impl Render for ToolUninjectResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let mut bits = self.removed_composer.clone();
        for ext in &self.removed_extensions {
            bits.push(format!("ext-{ext}"));
        }
        writeln!(w, "uninjected from {}: {}", self.package, bits.join(", "))
    }
}

pub fn run(format: OutputFormat, package: &str, with: &[String]) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let req = request::parse(package)?;
    let resolve_lock: &bougie_tool::install::LockResolver = &|paths, project_root| {
        super::composer_update::resolve_and_write_lock(paths, project_root).map(|_| ())
    };
    let php_installer = super::tool_callbacks::php_installer();
    let classifier = super::tool_callbacks::extension_classifier();
    let ext_installer = super::tool_callbacks::extension_installer();
    let php_requirement = super::tool_callbacks::required_php_fetcher();
    let ctx = InstallContext {
        paths: &paths,
        resolve_lock,
        php_installer: php_installer.as_ref(),
        classifier: classifier.as_ref(),
        ext_installer: ext_installer.as_ref(),
        php_requirement: php_requirement.as_ref(),
    };
    let outcome = inject::uninject(&ctx, &req.package(), with)?;
    emit(
        format,
        &ToolUninjectResult {
            schema_version: 1,
            package: outcome.package,
            tool_dir: outcome.tool_dir,
            removed_composer: outcome.removed_composer,
            removed_extensions: outcome.removed_extensions,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}
