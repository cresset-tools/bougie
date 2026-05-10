use crate::cli::OutputFormat;
use crate::commands::sync;
use crate::errors::BougieError;
use eyre::{eyre, Result};
use std::os::unix::process::CommandExt;
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
    let resolved_marker = project_root.join(".bougie").join("state").join("resolved");
    if !no_sync && !resolved_marker.exists() {
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
