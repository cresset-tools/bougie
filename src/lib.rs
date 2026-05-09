pub mod cli;
pub mod commands;
pub mod config;
pub mod errors;
pub mod fetch;
pub mod index;
pub mod install;
pub mod lock;
pub mod output;
pub mod paths;
pub mod request;
pub mod resolve;
pub mod shim;
pub mod state;
pub mod store;
pub mod target;
pub mod version;

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
        Command::Init { toml } => commands::init::run(format, field, toml),
        Command::Sync { offline: _, dry_run } => {
            commands::sync::run(format, field, dry_run)
        }
        Command::Cache(CacheCommand::Dir) => commands::cache_dir::run(format, field),
        Command::Php(PhpCommand::Dir) => commands::php_dir::run(format, field),
        Command::Php(PhpCommand::Install { request, flavor }) => commands::php_install::run(
            format,
            field,
            request.as_deref(),
            flavor.as_deref(),
        ),
        Command::Php(PhpCommand::Uninstall { request, flavor }) => commands::php_uninstall::run(
            format,
            field,
            &request,
            flavor.as_deref(),
        ),
        Command::Php(PhpCommand::List { .. }) => commands::php_list::run(format, field),
        Command::Php(PhpCommand::Find { request }) => {
            commands::php_find::run(format, field, request.as_deref())
        }
        Command::SelfCmd(SelfCommand::Version { short }) => {
            commands::self_version::run(format, field, short)
        }
        cmd => {
            eprintln!("not yet implemented: {cmd:?}");
            Ok(ExitCode::from(1))
        }
    }
}
