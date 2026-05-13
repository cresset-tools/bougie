//! `bougie server hosts add/remove/apply` — `/etc/hosts` sentinel-block
//! management. Lands in phase 5. Stubs here so `--help` advertises the
//! commands.

use crate::cli::OutputFormat;
use eyre::Result;
use std::process::ExitCode;

fn unimplemented(verb: &str) -> Result<ExitCode> {
    Err(eyre::eyre!(
        "bougie server hosts {verb} is not implemented yet (phase 5)"
    ))
}

pub fn add(_format: OutputFormat, _field: Option<&str>, _name: &str) -> Result<ExitCode> {
    unimplemented("add")
}

pub fn remove(_format: OutputFormat, _field: Option<&str>, _name: &str) -> Result<ExitCode> {
    unimplemented("remove")
}

pub fn apply(_format: OutputFormat, _field: Option<&str>) -> Result<ExitCode> {
    unimplemented("apply")
}
