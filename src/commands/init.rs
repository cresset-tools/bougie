use crate::cli::OutputFormat;
use crate::config::write_bougie_toml_skeleton;
use crate::output::{emit, Render};
use eyre::{Result, WrapErr};
use serde::Serialize;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

/// Phase 4 will refresh the cached index to learn the actual latest
/// minor. Until then `bougie init` writes a sensible default that can
/// be tightened later by the user (or by a future `bougie sync`).
const LATEST_PHP_MINOR: &str = "8.4";

#[derive(Debug, Serialize)]
pub struct InitResult {
    pub schema_version: u32,
    pub created: Vec<PathBuf>,
    pub already_present: Vec<PathBuf>,
}

impl Render for InitResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for p in &self.created {
            writeln!(w, "created  {}", p.display())?;
        }
        for p in &self.already_present {
            writeln!(w, "kept     {}", p.display())?;
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat, field: Option<&str>, with_toml: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir().wrap_err("getting current directory")?;
    let mut created = Vec::new();
    let mut already = Vec::new();

    let composer = cwd.join("composer.json");
    if composer.exists() {
        already.push(PathBuf::from("composer.json"));
    } else {
        fs::write(&composer, default_composer_json()).wrap_err("writing composer.json")?;
        created.push(PathBuf::from("composer.json"));
    }

    let bougie_dir = cwd.join(".bougie");
    for sub in ["conf.d", "bin", "state"] {
        let p = bougie_dir.join(sub);
        if !p.exists() {
            fs::create_dir_all(&p)
                .wrap_err_with(|| format!("creating {}", p.display()))?;
            created.push(PathBuf::from(".bougie").join(sub));
        }
    }

    let gitignore = bougie_dir.join(".gitignore");
    if !gitignore.exists() {
        fs::write(&gitignore, "bin/\nstate/\n").wrap_err("writing .bougie/.gitignore")?;
        created.push(PathBuf::from(".bougie").join(".gitignore"));
    }

    if with_toml {
        let toml_path = cwd.join("bougie.toml");
        if toml_path.exists() {
            already.push(PathBuf::from("bougie.toml"));
        } else {
            fs::write(&toml_path, write_bougie_toml_skeleton())
                .wrap_err("writing bougie.toml")?;
            created.push(PathBuf::from("bougie.toml"));
        }
    }

    let result = InitResult {
        schema_version: 1,
        created,
        already_present: already,
    };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn default_composer_json() -> String {
    let value = serde_json::json!({
        "require": {
            "php": format!("^{LATEST_PHP_MINOR}")
        }
    });
    let mut s = serde_json::to_string_pretty(&value).expect("infallible serialize");
    s.push('\n');
    s
}
