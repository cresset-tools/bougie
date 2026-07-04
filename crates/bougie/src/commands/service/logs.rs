//! `bougie service logs [-f] [-n N] [<name>]`. CLI.md §3.8.8.
//!
//! With a name, tails (and optionally follows) that one service. With no
//! name, tails the combined ("multilog") stream of every service the
//! project declares — the same view `bougie up` attaches to — with each
//! line prefixed by its (colorized, on a TTY) service name.

use super::client;
use super::config_mut::locate_project_root;
use bougie_cli::OutputFormat;
use bougie_config::load_project;
use bougie_paths::Paths;
use eyre::{eyre, Result};
use serde_json::json;
use std::io::IsTerminal;
use std::process::ExitCode;

pub fn run(
    _format: OutputFormat,
    name: Option<String>,
    follow: bool,
    lines: usize,
) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let args = match name {
        Some(service) => json!({
            "service": service,
            "lines": lines,
            "follow": follow,
        }),
        None => {
            // No service named: tail every service declared in the
            // project as one combined stream. Resolve the declarations
            // here so the daemon just gets the concrete list.
            let project_root = locate_project_root()?;
            let project = load_project(&project_root)?;
            let services: Vec<String> = project.bougie.services.keys().cloned().collect();
            if services.is_empty() {
                return Err(eyre!(
                    "no services declared in this project (try `bougie service add <name>` first)"
                ));
            }
            json!({
                "services": services,
                "lines": lines,
                "follow": follow,
                // Colorize the per-service prefixes when our stdout is a
                // terminal; the daemon writes the ANSI codes.
                "color": std::io::stdout().is_terminal(),
            })
        }
    };
    // `service.logs` is the only streaming method today: the daemon
    // emits progress frames with the tailed bytes and, in follow
    // mode, never sends a terminal result. The client closes on
    // SIGINT (Ctrl-C) to end follow.
    client::call_streaming(&paths, "service.logs", args)?;
    Ok(ExitCode::SUCCESS)
}
