//! `bgx <vendor/name>[@<constraint>] [--php <ver>] [--with <pkg>...] -- args...`
//!
//! Short alias binary for `bougie tool run`. Rebuilds argv as
//! `["bougie", "tool", "run", <user args...>]`, parses with clap, and
//! hands off to the same `bougie::run` dispatcher as the main binary.
//! Shipped as a separate `[[bin]]` rather than an argv0 symlink so
//! Windows works without symlink permissions and so
//! `bougie self update`-style binary relocations don't break the
//! shim (see `TOOL_PLAN.md` §`bgx`).

use bougie::{Cli, exit_code_for};
use clap::Parser;
use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let user_argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut argv: Vec<OsString> = Vec::with_capacity(user_argv.len() + 3);
    argv.push(OsString::from("bougie"));
    argv.push(OsString::from("tool"));
    argv.push(OsString::from("run"));
    argv.extend(user_argv);

    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(e) => {
            // clap renders its own errors to stderr (or stdout for
            // `--help`) and gives us the right exit code.
            e.exit();
        }
    };
    match bougie::run(cli) {
        Ok(code) => code,
        Err(err) => {
            report_error(&err);
            ExitCode::from(exit_code_for(&err))
        }
    }
}

/// Mirror of `bougie::main::report_error` — same uv-style format so
/// `bgx` and `bougie` look the same on failure. Kept inline rather
/// than re-exported to keep `bougie::main`'s helpers private.
fn report_error(err: &eyre::Report) {
    let mut chain = err.chain();
    let Some(head) = chain.next() else {
        return;
    };
    let head = head.to_string();
    let mut lines = head.lines();
    if let Some(first) = lines.next() {
        eprintln!("error: {first}");
    }
    for rest in lines {
        eprintln!("       {rest}");
    }
    for cause in chain {
        let s = cause.to_string();
        let mut cl = s.lines();
        if let Some(first) = cl.next() {
            eprintln!("  caused by: {first}");
        }
        for rest in cl {
            eprintln!("             {rest}");
        }
    }
}
