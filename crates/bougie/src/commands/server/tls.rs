//! `bougie server tls install/uninstall` — local CA via the
//! bougie-distributed mkcert tool. Lands in phase 7. Stubs here so the
//! command is discoverable via `--help` even before the implementation.

use crate::cli::OutputFormat;
use eyre::Result;
use std::process::ExitCode;

fn unimplemented(verb: &str) -> Result<ExitCode> {
    Err(eyre::eyre!(
        "bougie server tls {verb} is not implemented yet (phase 7)"
    ))
}

pub fn install(_format: OutputFormat) -> Result<ExitCode> {
    unimplemented("install")
}

pub fn uninstall(_format: OutputFormat) -> Result<ExitCode> {
    unimplemented("uninstall")
}
