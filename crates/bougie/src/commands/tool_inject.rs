//! `bougie tool inject <vendor/name> --with <extra>...`.

use bougie_cli::OutputFormat;
use bougie_installer::install::install_php;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use bougie_resolver::ResolveOptions;
use bougie_tool::classify::ExtensionClassifier;
use bougie_tool::install::{ExtInstaller, InstallContext};
use bougie_tool::resolve::{PhpChoice, PhpInstaller};
use bougie_tool::{inject, request};
use bougie_version::request::parse_request as parse_php_request;
use eyre::{Result, WrapErr};
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
            "PHP extension `--with {name}` is not wired yet; \
             use `vendor/name` for composer packages instead"
        ))
    };
    let ctx = InstallContext {
        paths: &paths,
        resolve_lock,
        php_installer,
        classifier,
        ext_installer,
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
