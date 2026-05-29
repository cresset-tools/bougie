//! `bgx <vendor/name>[@<constraint>] [--php <ver>] [--with <pkg>...] -- args...`
//!
//! Thin exec-shim for `bougie tool run`. Ported almost verbatim from
//! `crates/uv/src/bin/uvx.rs` upstream — same structure, same
//! `try_exists` semantics, same versioned-suffix lookup
//! (`bgx@1.2.3` finds `bougie@1.2.3`). Differences vs uvx:
//!
//! - We invoke `bougie tool run <args>` rather than `uv tool uvx
//!   <args>` because there's no dedicated `tool bgx` subcommand on
//!   the bougie side — `tool run` is the right dispatch path.
//! - Windows uses `std::process::Command::status` rather than the
//!   custom `uv_windows::spawn_child` (which handles Ctrl-C
//!   propagation specially). Revisit when Phase 4 lands.
//!
//! Deliberately no dependency on the `bougie` library crate.
//! Linking `bougie::{Cli, run}` would pull the whole workspace and
//! bloat the binary to ~16 MB; this exec-shim stays under 350 KB
//! stripped.

use std::convert::Infallible;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

/// Spawns a command exec-style. On Unix, replaces this process with
/// the child via `execve` so signals + exit code pass through
/// transparently. On Windows there's no execve; spawn + wait +
/// propagate.
fn exec_spawn(cmd: &mut Command) -> std::io::Result<Infallible> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(err)
    }
    #[cfg(not(unix))]
    {
        // No execve on Windows. Spawn + wait + exit with the
        // child's code directly — same shape as uv's
        // `uv_windows::spawn_child`, which also never returns
        // `Ok` (the return type is `Result<Infallible>`). Custom
        // Ctrl-C / job-object handling lands in Phase 4.
        let status = cmd.status()?;
        let code = i32::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1));
        std::process::exit(code);
    }
}

/// Assuming the binary is called something like `bgx@1.2.3(.exe)`,
/// compute the `@1.2.3(.exe)` part so we can preferentially find
/// `bougie@1.2.3(.exe)`, for folks who like managing multiple
/// installs in this way.
fn get_bgx_suffix(current_exe: &Path) -> Option<&str> {
    let os_file_name = current_exe.file_name()?;
    let file_name_str = os_file_name.to_str()?;
    file_name_str.strip_prefix("bgx")
}

/// Gets the path to `bougie`, given info about `bgx`.
fn get_bougie_path(
    current_exe_parent: &Path,
    bgx_suffix: Option<&str>,
) -> std::io::Result<PathBuf> {
    // First try to find a matching suffixed `bougie`, e.g.
    // `bougie@1.2.3(.exe)`.
    let bougie_with_suffix =
        bgx_suffix.map(|suffix| current_exe_parent.join(format!("bougie{suffix}")));
    if let Some(bougie_with_suffix) = &bougie_with_suffix {
        match bougie_with_suffix.try_exists() {
            Ok(true) => return Ok(bougie_with_suffix.to_owned()),
            Ok(false) => { /* definitely not there, proceed to fallback */ }
            Err(err) => {
                // We don't know if `bougie@1.2.3` exists, something
                // errored when checking. We *could* blindly use
                // `bougie@1.2.3` here, but in this narrow corner
                // case it's probably better to default to plain
                // `bougie` so we don't mess up users who weren't
                // using suffixes.
                eprintln!(
                    "warning: failed to determine if `{}` exists, trying `bougie` instead: {err}",
                    bougie_with_suffix.display()
                );
            }
        }
    }

    // Then just look for good ol' `bougie`.
    let bougie = current_exe_parent.join(format!("bougie{}", std::env::consts::EXE_SUFFIX));
    // If we are sure the `bougie` binary does not exist, display a
    // clearer error message. If we're not certain
    // (`try_exists() == Err`), keep going and hope it works.
    if matches!(bougie.try_exists(), Ok(false)) {
        let message = if let Some(bougie_with_suffix) = bougie_with_suffix {
            format!(
                "Could not find the `bougie` binary at either of:\n  {}\n  {}",
                bougie_with_suffix.display(),
                bougie.display(),
            )
        } else {
            format!("Could not find the `bougie` binary at: {}", bougie.display())
        };
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, message))
    } else {
        Ok(bougie)
    }
}

fn run() -> std::io::Result<ExitStatus> {
    let current_exe = std::env::current_exe()?;
    let Some(bin) = current_exe.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not determine the location of the `bgx` binary",
        ));
    };
    let bgx_suffix = get_bgx_suffix(&current_exe);
    let bougie = get_bougie_path(bin, bgx_suffix)?;
    // Dispatch into the hidden `bgx` subcommand on the bougie side
    // rather than `tool run`. Both do exactly the same work, but
    // the hidden variant renders `--help` and clap errors with
    // `bgx` as the program name. Mirrors uv's `uv tool uvx`
    // alongside `uv tool run`.
    let args = ["tool", "bgx"]
        .iter()
        .map(OsString::from)
        // Skip the `bgx` name
        .chain(std::env::args_os().skip(1))
        .collect::<Vec<_>>();

    let mut cmd = Command::new(bougie);
    cmd.args(&args);
    match exec_spawn(&mut cmd)? {}
}

fn main() -> ExitCode {
    let result = run();
    match result {
        // Fail with 2 if the status cannot be cast to an exit code.
        Ok(status) => u8::try_from(status.code().unwrap_or(2))
            .unwrap_or(2)
            .into(),
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(2)
        }
    }
}
