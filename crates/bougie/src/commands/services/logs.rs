//! `bougie services logs [-f] [-n N] <name>`. CLI.md §3.8.8.

use super::client;
use bougie_cli::OutputFormat;
use bougie_paths::Paths;
use eyre::Result;
use serde_json::json;
use std::process::ExitCode;

pub fn run(
    _format: OutputFormat,
    name: String,
    follow: bool,
    lines: usize,
) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let args = json!({
        "service": name,
        "lines": lines,
        "follow": follow,
    });
    // `service.logs` is the only streaming method today: the daemon
    // emits progress frames with the tailed bytes and, in follow
    // mode, never sends a terminal result. The client closes on
    // SIGINT (Ctrl-C) to end follow.
    client::call_streaming(&paths, "service.logs", args)?;
    Ok(ExitCode::SUCCESS)
}
