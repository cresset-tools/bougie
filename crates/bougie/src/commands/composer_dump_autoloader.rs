//! `bougie composer dump-autoloader` — regenerate
//! `vendor/composer/autoload_*.php` against the current
//! `composer.lock`. Drop-in for upstream `composer dump-autoload`;
//! output is byte-equivalent to Composer 2.8.12 with the same flags.
//!
//! Resolves the working directory the same way Composer's
//! `--working-dir` flag does: explicit `-d` / `--working-dir` wins,
//! otherwise CWD. We don't walk upwards to find a project root — if
//! the user is in `src/`, they should `cd ..` or pass `-d`, same as
//! Composer. The directory is required to contain both
//! `composer.json` (read for `config.autoloader-suffix`) and
//! `composer.lock` (read for the package list + content-hash).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bougie_autoloader::{dump_autoload, DumpRequest};
use bougie_cli::OutputFormat;
use bougie_output::output::{emit, Render};
use eyre::{Context, Result};
use serde::Serialize;

/// JSON-v1 output shape. Echoes back the flags that were applied so a
/// scripted caller can confirm what bougie ran with (matters for
/// `--apcu-autoloader` without an explicit prefix — the result will
/// carry the random prefix that was generated).
#[derive(Debug, Serialize)]
pub struct DumpAutoloaderResult {
    pub schema_version: u32,
    pub project_root: PathBuf,
    pub optimize: bool,
    pub classmap_authoritative: bool,
    pub no_dev: bool,
    pub apcu_autoloader: bool,
    pub autoloader_suffix: Option<String>,
}

impl Render for DumpAutoloaderResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let mut active = Vec::new();
        if self.optimize {
            active.push("optimize");
        }
        if self.classmap_authoritative {
            active.push("classmap-authoritative");
        }
        if self.no_dev {
            active.push("no-dev");
        }
        if self.apcu_autoloader {
            active.push("apcu-autoloader");
        }
        if self.autoloader_suffix.is_some() {
            active.push("autoloader-suffix");
        }
        let suffix_note = if active.is_empty() {
            String::new()
        } else {
            format!(" ({})", active.join(", "))
        };
        writeln!(
            w,
            "wrote vendor/composer/autoload_*.php in {}{suffix_note}",
            self.project_root.display()
        )
    }
}

#[allow(clippy::fn_params_excessive_bools)] // names line up with Composer CLI flags 1:1
pub fn run(
    format: OutputFormat,
    working_dir: Option<PathBuf>,
    optimize: bool,
    classmap_authoritative: bool,
    no_dev: bool,
    apcu_autoloader: bool,
    apcu_prefix: Option<String>,
    autoloader_suffix: Option<String>,
) -> Result<ExitCode> {
    let project_root = match working_dir {
        Some(p) => p,
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };

    require_file(&project_root, "composer.json")?;
    require_file(&project_root, "composer.lock")?;

    // An explicit `--apcu-autoloader-prefix` implies `--apcu-autoloader`
    // (Composer does the same — see commands/InstallCommand.php).
    let apcu_autoloader = apcu_autoloader || apcu_prefix.is_some();

    let req = DumpRequest {
        project_root: &project_root,
        optimize,
        classmap_authoritative,
        no_dev,
        apcu_autoloader,
        apcu_prefix,
        autoloader_suffix: autoloader_suffix.clone(),
    };
    dump_autoload(&req).wrap_err("dump_autoload failed")?;

    let result = DumpAutoloaderResult {
        schema_version: 1,
        project_root,
        optimize,
        classmap_authoritative,
        no_dev,
        apcu_autoloader,
        autoloader_suffix,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn require_file(dir: &Path, name: &str) -> Result<()> {
    let path = dir.join(name);
    if !path.is_file() {
        return Err(eyre::eyre!(
            "{name} not found in {}",
            dir.display()
        ));
    }
    Ok(())
}
