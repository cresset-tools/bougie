use bougie::{shim, Cli};
use clap::Parser;
use eyre::Result;
use std::process::ExitCode;

fn main() -> Result<ExitCode> {
    color_eyre::install()?;

    let argv0 = std::env::args_os().next().unwrap_or_default();
    if let Some(role) = shim::role_from_argv0(&argv0) {
        return shim::exec(role);
    }

    let cli = Cli::parse();
    bougie::run(cli)
}
