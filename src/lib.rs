pub mod cli;
pub mod errors;
pub mod output;
pub mod paths;
pub mod target;

pub use cli::{Cli, Command, OutputFormat};
pub use errors::{exit_code_for, BougieError};
pub use paths::Paths;
pub use target::Triple;

use eyre::Result;
use std::process::ExitCode;

pub fn run(cli: Cli) -> Result<ExitCode> {
    let Cli { command, .. } = cli;
    eprintln!("not yet implemented: {command:?}");
    Ok(ExitCode::from(1))
}
