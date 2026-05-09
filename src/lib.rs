pub mod cli;
pub mod commands;
pub mod errors;
pub mod output;
pub mod paths;
pub mod shim;
pub mod target;

pub use cli::{Cli, Command, OutputFormat};
pub use errors::{exit_code_for, BougieError};
pub use paths::Paths;
pub use target::Triple;

use cli::{CacheCommand, PhpCommand, SelfCommand};
use eyre::Result;
use std::process::ExitCode;

pub fn run(cli: Cli) -> Result<ExitCode> {
    let format = cli.format;
    let field = cli.field.as_deref();

    match cli.command {
        Command::Cache(CacheCommand::Dir) => commands::cache_dir::run(format, field),
        Command::Php(PhpCommand::Dir) => commands::php_dir::run(format, field),
        Command::SelfCmd(SelfCommand::Version { short }) => {
            commands::self_version::run(format, field, short)
        }
        cmd => {
            eprintln!("not yet implemented: {cmd:?}");
            Ok(ExitCode::from(1))
        }
    }
}
