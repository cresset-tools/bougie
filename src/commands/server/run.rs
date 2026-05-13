//! Foreground entry point. Lands in phase 1; phase 0 only validates
//! flags and errors out with a clear "not yet implemented" message so
//! users can already discover the command via `--help`.

use crate::cli::OutputFormat;
use eyre::Result;
use std::path::Path;
use std::process::ExitCode;

use super::config;

pub fn run(
    _format: OutputFormat,
    _field: Option<&str>,
    config_override: Option<&Path>,
    _listen: Option<&str>,
    _log_format: Option<&str>,
) -> Result<ExitCode> {
    // Resolve the config path so a typo in `--config` fails early with
    // the same error a real run would produce in phase 1.
    let path = config::resolve_path(config_override)?;
    let _cfg = config::load(&path)?;
    Err(eyre::eyre!(
        "bougie server (foreground run) is not implemented yet — the HTTP listener lands in phase 1.\n\
         Configuration in {} validated successfully; use `bougie server add/remove/list` to manage hosts.",
        path.display()
    ))
}
