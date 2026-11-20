//! `bougie tool-exec <wrapper-path> [args...]` — runtime shim invoked
//! by tool wrappers via the shebang line.
//!
//! Phase 1 wiring: hand off to `bougie_tool::exec`, which validates the
//! wrapper path, loads the receipt, and `execve`s the pinned PHP. On
//! Unix the only way this function returns is on a prepare-time error
//! or an `execve` failure.

use bougie_cli::{Cli, Command, OutputFormat};
use bougie_paths::Paths;
use bougie_tool::exec;
use eyre::Result;
use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

/// Build the [`Cli`] for a shebang-driven `tool-exec` invocation
/// straight from raw argv, bypassing clap entirely.
///
/// Everything after the wrapper path belongs to the *tool*. clap's
/// `trailing_var_arg` only stops flag parsing after the first
/// non-flag value, so a leading token that exactly matches a
/// registered flag still gets claimed by clap — `magequery --help`
/// printed the hidden tool-exec help instead of the tool's own, and a
/// leading `-q`/`-v`/`--format` would be swallowed as bougie's global
/// flags the same way.
///
/// Returns `Some` only for the argv shape a shebang produces:
/// `bougie tool-exec <wrapper> [args…]`, where the kernel-supplied
/// wrapper path never starts with `-`. Hand-typed edge cases (bare
/// `bougie tool-exec`, `bougie tool-exec --help`) return `None` and
/// fall through to clap for its usage/help rendering.
pub fn cli_from_argv<I>(argv: I) -> Option<Cli>
where
    I: IntoIterator<Item = OsString>,
{
    let mut argv = argv.into_iter();
    let _argv0 = argv.next()?;
    if argv.next()? != "tool-exec" {
        return None;
    }
    let wrapper = argv.next()?;
    if wrapper.as_encoded_bytes().first() == Some(&b'-') {
        return None;
    }
    Some(Cli {
        command: Command::ToolExec {
            wrapper: wrapper.into(),
            args: argv.collect(),
        },
        quiet: false,
        verbose: false,
        format: OutputFormat::Text,
    })
}

pub fn run(wrapper: &Path, args: Vec<OsString>) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let prep = exec::prepare(&paths, wrapper, args)?;
    // `execve_replace` returns Err only when prep is valid but execve
    // itself fails. On success it never returns.
    let _: std::convert::Infallible = exec::execve_replace(&prep)?;
    unreachable!("execve_replace returns Infallible on success");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn shebang_invocation_forwards_leading_flags_verbatim() {
        // `magequery --help -v --format json` — every token after the
        // wrapper reaches the tool, even ones that collide with
        // bougie's own registered flags.
        let cli = cli_from_argv(argv(&[
            "bougie",
            "tool-exec",
            "/tools/cresset-magequery/bin/magequery",
            "--help",
            "-v",
            "--format",
            "json",
        ]))
        .unwrap();
        match cli.command {
            Command::ToolExec { wrapper, args } => {
                assert_eq!(
                    wrapper,
                    std::path::PathBuf::from("/tools/cresset-magequery/bin/magequery")
                );
                assert_eq!(args, argv(&["--help", "-v", "--format", "json"]));
            }
            other => panic!("expected ToolExec, got {other:?}"),
        }
        assert!(!cli.quiet && !cli.verbose);
    }

    #[test]
    fn non_tool_exec_argv_falls_through_to_clap() {
        assert!(cli_from_argv(argv(&["bougie", "sync"])).is_none());
        assert!(cli_from_argv(argv(&["bougie"])).is_none());
        assert!(cli_from_argv(argv(&[])).is_none());
    }

    #[test]
    fn hand_typed_edge_cases_fall_through_to_clap() {
        // Bare `bougie tool-exec` keeps clap's missing-<WRAPPER> usage
        // error; a leading `-` token (`bougie tool-exec --help`) keeps
        // clap's help — a shebang-supplied wrapper path never starts
        // with `-`.
        assert!(cli_from_argv(argv(&["bougie", "tool-exec"])).is_none());
        assert!(cli_from_argv(argv(&["bougie", "tool-exec", "--help"])).is_none());
    }
}
