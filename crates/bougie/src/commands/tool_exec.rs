//! `bougie tool-exec <wrapper-path> [args...]` — runtime shim invoked
//! by tool wrappers via the shebang line.
//!
//! Phase 1 wiring: hand off to `bougie_tool::exec`, which validates the
//! wrapper path, loads the receipt, and `execve`s the pinned PHP. On
//! Unix the only way this function returns is on a prepare-time error
//! or an `execve` failure.

use bougie_paths::Paths;
use bougie_tool::exec;
use eyre::Result;
use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

pub fn run(wrapper: &Path, args: Vec<OsString>) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let prep = exec::prepare(&paths, wrapper, args)?;
    // `execve_replace` returns Err only when prep is valid but execve
    // itself fails. On success it never returns.
    let _: std::convert::Infallible = exec::execve_replace(&prep)?;
    unreachable!("execve_replace returns Infallible on success");
}
