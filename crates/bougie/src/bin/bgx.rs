//! `bgx <vendor/name>[@<constraint>] [--php <ver>] [--with <pkg>...] -- args...`
//!
//! Thin exec-shim for `bougie tool run`. Finds the colocated
//! `bougie` binary (same directory as this `bgx`, else PATH lookup)
//! and execve's it with `["tool", "run", ...orig_args]`.
//!
//! Deliberately keeps zero dependencies on the `bougie` library
//! crate. Linking `bougie::{Cli, run}` here would pull in the whole
//! workspace (composer resolver, index, daemon, server, …) and
//! bloat the binary to ~16 MB. As an exec-shim, `bgx` stays under
//! a megabyte stripped and the actual work runs inside `bougie`
//! itself.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let bougie = match locate_bougie() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            // Exit 2 distinguishes "the shim couldn't even find
            // bougie" from "bougie ran and reported exit 1". Matches
            // uv's uvx convention.
            return ExitCode::from(2);
        }
    };

    let mut args: Vec<OsString> = Vec::new();
    args.push(OsString::from("tool"));
    args.push(OsString::from("run"));
    args.extend(std::env::args_os().skip(1));

    exec(&bougie, &args)
}

/// Resolve the `bougie` binary `bgx` should hand off to.
///
/// Priority:
///   1. `BGX_BOUGIE` env override — escape hatch for test harnesses
///      and for users running mismatched binaries side-by-side.
///   2. `<bgx's dir>/bougie[.exe]` — the normal install layout:
///      both binaries land in the same directory.
///   3. `PATH` lookup via `which`-style traversal — last resort so
///      `bgx` keeps working if someone installs only it.
fn locate_bougie() -> Result<PathBuf, String> {
    if let Some(env) = std::env::var_os("BGX_BOUGIE") {
        return Ok(PathBuf::from(env));
    }

    let bougie_name: &str = if cfg!(windows) { "bougie.exe" } else { "bougie" };

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(bougie_name);
        // `try_exists` distinguishes "definitely not there"
        // (Ok(false), fall through) from "couldn't check"
        // (Err, optimistically try anyway — exec will surface a
        // useful error if it really isn't there). uvx uses the
        // same trick.
        match candidate.try_exists() {
            Ok(true) => return Ok(candidate),
            Ok(false) => {}
            Err(_) => return Ok(candidate),
        }
    }

    let Some(path) = std::env::var_os("PATH") else {
        return Err(
            "could not find `bougie` next to `bgx` and PATH is unset; \
             install bougie or set BGX_BOUGIE to its path"
                .into(),
        );
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bougie_name);
        match candidate.try_exists() {
            Ok(true) => return Ok(candidate),
            Ok(false) => {}
            Err(_) => return Ok(candidate),
        }
    }
    Err(format!(
        "could not find `{bougie_name}` next to `bgx` or on PATH; \
         set BGX_BOUGIE to its path"
    ))
}

#[cfg(unix)]
fn exec(bougie: &std::path::Path, args: &[OsString]) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(bougie).args(args).exec();
    eprintln!("error: exec {}: {err}", bougie.display());
    ExitCode::from(1)
}

#[cfg(not(unix))]
fn exec(bougie: &std::path::Path, args: &[OsString]) -> ExitCode {
    let status = match std::process::Command::new(bougie).args(args).status() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: spawn {}: {e}", bougie.display());
            return ExitCode::from(1);
        }
    };
    let code = status.code().unwrap_or(1);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
