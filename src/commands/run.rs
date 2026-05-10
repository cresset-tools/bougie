use crate::cli::OutputFormat;
use crate::commands::sync;
use crate::errors::BougieError;
use crate::paths::Paths;
use crate::state::{read_project_resolved, read_project_resolved_composer};
use eyre::{eyre, Result};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::ExitCode;

/// `bougie run [--] <cmd> [args...]` — set `PATH` and `PHP_INI_SCAN_DIR`,
/// then exec the requested command. Per CLI.md §3.4, implicitly runs
/// `bougie sync` first unless `--no-sync` is passed.
pub fn run(
    _with: &[String],
    argv: &[String],
    format: OutputFormat,
    field: Option<&str>,
    no_sync: bool,
) -> Result<ExitCode> {
    if argv.is_empty() {
        return Err(eyre!("nothing to run"));
    }
    let project_root = std::env::current_dir()?;
    if !no_sync && !is_environment_present(&project_root)? {
        sync::run(format, field, false)?;
    }
    let bougie_bin = project_root.join(".bougie").join("bin");
    let conf_d = project_root.join(".bougie").join("conf.d");

    let prev_path = std::env::var("PATH").unwrap_or_default();
    let new_path = if bougie_bin.exists() {
        format!("{}:{prev_path}", bougie_bin.display())
    } else {
        prev_path
    };

    let (program, rest) = argv.split_first().ok_or_else(|| eyre!("argv missing"))?;
    let err = std::process::Command::new(program)
        .args(rest)
        .env("PATH", new_path)
        .env("PHP_INI_SCAN_DIR", &conf_d)
        .env("BOUGIE_PROJECT_ROOT", &project_root)
        .exec();
    Err(BougieError::Filesystem {
        operation: format!("execve {program}"),
        detail: err.to_string(),
    }
    .into())
}

/// True iff the project's resolved markers point at on-disk artifacts
/// that still exist. Used to decide whether the implicit-sync step is
/// needed — a missing marker, missing install dir, or missing composer
/// phar all warrant resyncing.
fn is_environment_present(project_root: &Path) -> Result<bool> {
    let paths = Paths::from_env()?;

    let Ok((version, flavor)) = read_project_resolved(project_root) else {
        return Ok(false);
    };
    let install = paths.installs().join(format!("{version}-{flavor}"));
    if !install.join("bin").join("php").exists() {
        return Ok(false);
    }

    let Ok(composer_version) = read_project_resolved_composer(project_root) else {
        return Ok(false);
    };
    if !paths.composer_phar(&composer_version).exists() {
        return Ok(false);
    }
    Ok(true)
}
