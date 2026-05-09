use bougie::Cli;
use clap::Parser;
use eyre::Result;
use std::process::ExitCode;

fn main() -> Result<ExitCode> {
    color_eyre::install()?;
    let cli = Cli::parse();
    bougie::run(cli)
}
