use crate::errors::BougieError;
use eyre::{eyre, Result};
use std::os::unix::process::CommandExt;
use std::process::ExitCode;

/// `bougie run -- <cmd> [args...]` — set `PATH` and `PHP_INI_SCAN_DIR`,
/// then exec the requested command.
///
/// Phase 8 minimum: prepend `<project>/.bougie/bin` to `PATH` if it
/// exists and set `PHP_INI_SCAN_DIR`. The implicit-sync flow from §3.4
/// ("Implicitly runs `bougie sync` first unless `--no-sync` is passed")
/// requires phase 7's sync to be re-entrant; for v0.1 we leave the
/// implicit-sync to the user.
pub fn run(_with: &[String], argv: &[String]) -> Result<ExitCode> {
    if argv.is_empty() {
        return Err(eyre!("nothing to run"));
    }
    let project_root = std::env::current_dir()?;
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
        .exec();
    Err(BougieError::Filesystem(err.to_string()).into())
}
