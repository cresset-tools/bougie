use bougie_cli::OutputFormat;
use bougie_config::write_bougie_toml_skeleton;
use bougie_output::output::{emit, Render};
use eyre::{Result, WrapErr};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct PinResult {
    pub schema_version: u32,
    pub target: PathBuf,
    pub written: String,
}

impl Render for PinResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "pinned php to {} in {}", self.written, self.target.display())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinTarget {
    Auto,
    Toml,
    Composer,
}

pub fn run(
    format: OutputFormat,
        request: &str,
    pin_target: PinTarget,
) -> Result<ExitCode> {
    let project_root = std::env::current_dir()?;
    let toml_path = project_root.join("bougie.toml");
    let composer_path = project_root.join("composer.json");

    let dest = match (pin_target, toml_path.exists(), composer_path.exists()) {
        (PinTarget::Toml, _, _)
        | (PinTarget::Auto, true, _)
        | (PinTarget::Auto, false, false) => Target::Toml(toml_path),
        (PinTarget::Composer, _, true) | (PinTarget::Auto, false, true) => {
            Target::Composer(composer_path)
        }
        (PinTarget::Composer, _, false) => {
            return Err(eyre::eyre!(
                "no composer.json in {}",
                project_root.display()
            ))
        }
    };

    let written_path = match dest {
        Target::Toml(path) => write_toml_pin(&path, request)?,
        Target::Composer(path) => write_composer_pin(&path, request)?,
    };
    let result = PinResult {
        schema_version: 1,
        target: written_path,
        written: request.to_owned(),
    };
    emit(format, &result)?;

    // A pin only takes effect once the toolchain is re-resolved: the new
    // minor has to select/download its interpreter, rebuild its per-minor
    // extension fragments, and (when it resolves to a *system* PHP) drop
    // the previous minor's managed conf.d. Without an auto-sync the next
    // `bougie run` keeps loading the old minor's ABI-bound `.so`s and PHP
    // errors on startup. Sync reads the freshly written pin from disk.
    crate::commands::sync::run(
        &project_root,
        format,
        false,
        false,
        None,
        None,
        bougie_cli::PhpPrefArgs::default(),
        bougie_composer_resolver::ResolutionStrategy::Highest,
    )
}

enum Target {
    Toml(PathBuf),
    Composer(PathBuf),
}

fn write_toml_pin(path: &std::path::Path, version: &str) -> Result<PathBuf> {
    let body = if path.exists() {
        std::fs::read_to_string(path)
            .wrap_err_with(|| format!("reading {}", path.display()))?
    } else {
        write_bougie_toml_skeleton()
    };
    let mut doc: toml_edit::DocumentMut = body
        .parse()
        .wrap_err_with(|| format!("parsing {}", path.display()))?;
    let php = doc
        .entry("php")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let table = php
        .as_table_mut()
        .ok_or_else(|| eyre::eyre!("[php] is not a table in {}", path.display()))?;
    table["version"] = toml_edit::value(version);
    std::fs::write(path, doc.to_string())
        .wrap_err_with(|| format!("writing {}", path.display()))?;
    Ok(path.to_path_buf())
}

fn write_composer_pin(path: &std::path::Path, version: &str) -> Result<PathBuf> {
    let body = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("reading {}", path.display()))?;
    let mut v: serde_json::Value =
        serde_json::from_str(&body).wrap_err_with(|| format!("parsing {}", path.display()))?;
    let extra = v
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("composer.json is not a JSON object"))?
        .entry("extra")
        .or_insert_with(|| serde_json::json!({}));
    let extra_obj = extra
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("`extra` is not an object"))?;
    let bougie = extra_obj
        .entry("bougie")
        .or_insert_with(|| serde_json::json!({}));
    let bougie_obj = bougie
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("`extra.bougie` is not an object"))?;
    let php = bougie_obj
        .entry("php")
        .or_insert_with(|| serde_json::json!({}));
    let php_obj = php
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("`extra.bougie.php` is not an object"))?;
    php_obj.insert("version".into(), serde_json::Value::String(version.into()));
    let mut s = serde_json::to_string_pretty(&v).wrap_err("encoding composer.json")?;
    s.push('\n');
    std::fs::write(path, s).wrap_err_with(|| format!("writing {}", path.display()))?;
    Ok(path.to_path_buf())
}
