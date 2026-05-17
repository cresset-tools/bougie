use bougie::{exit_code_for, shim, Cli};
use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    if let Some(role) = shim::role_from_argv0(&argv0) {
        return match shim::exec(role) {
            Ok(code) => code,
            Err(err) => {
                report_error(&err);
                ExitCode::from(exit_code_for(&err))
            }
        };
    }

    let cli = Cli::parse();
    match bougie::run(cli) {
        Ok(code) => code,
        Err(err) => {
            report_error(&err);
            ExitCode::from(exit_code_for(&err))
        }
    }
}

/// Render an error to stderr in uv's `error: <message>` style.
///
/// Multi-line error messages (from BougieError variants that include
/// hint blocks) keep their structure; subsequent lines are indented to
/// align under the first line. Cause chain entries follow on
/// `  caused by:` lines, mirroring uv.
fn report_error(err: &eyre::Report) {
    let mut chain = err.chain();
    let head = match chain.next() {
        Some(c) => c.to_string(),
        None => return,
    };
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
