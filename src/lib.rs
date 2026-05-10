pub mod cli;
pub mod commands;
pub mod composer;
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

use cli::{CacheCommand, ComposerCommand, PhpCommand, SelfCommand};
use eyre::Result;
use std::process::ExitCode;

pub fn run(cli: Cli) -> Result<ExitCode> {
    let format = cli.format;
    let field = cli.field.as_deref();

    match cli.command {
        Command::Init { toml } => commands::init::run(format, field, toml),
        Command::Sync { offline: _, dry_run } => commands::sync::run(format, field, dry_run),
        Command::Run { with, no_sync, argv } => {
            commands::run::run(&with, &argv, format, field, no_sync)
        }
        Command::Ext(cli::ExtCommand::Add { names, no_sync }) => {
            commands::ext_add_remove::add(format, field, names, no_sync)
        }
        Command::Ext(cli::ExtCommand::Remove { names, no_sync }) => {
            commands::ext_add_remove::remove(format, field, names, no_sync)
        }
        Command::Ext(cli::ExtCommand::List {
            only_installed,
            only_available,
            ..
        }) => commands::ext_list::run(format, field, only_installed, only_available),
        Command::Cache(CacheCommand::Dir) => commands::cache_dir::run(format, field),
        Command::Cache(CacheCommand::Clean) => commands::cache_clean::run(format, field),
        Command::Cache(CacheCommand::Size) => commands::cache_size::run(format, field),
        Command::Cache(CacheCommand::Prune { dry_run, prune_projects: _ }) => {
            commands::cache_prune::run(format, field, dry_run)
        }
        Command::Php(PhpCommand::Dir) => commands::php_dir::run(format, field),
        Command::Php(PhpCommand::Install { requests, flavor }) => commands::php_install::run(
            format,
            field,
            &requests,
            flavor.as_deref(),
        ),
        Command::Php(PhpCommand::Uninstall { requests, flavor }) => commands::php_uninstall::run(
            format,
            field,
            &requests,
            flavor.as_deref(),
        ),
        Command::Php(PhpCommand::List { .. }) => commands::php_list::run(format, field),
        Command::Php(PhpCommand::Find { request }) => {
            commands::php_find::run(format, field, request.as_deref())
        }
        Command::Php(PhpCommand::Pin { request, toml, composer }) => {
            let target = if toml {
                commands::php_pin::PinTarget::Toml
            } else if composer {
                commands::php_pin::PinTarget::Composer
            } else {
                commands::php_pin::PinTarget::Auto
            };
            commands::php_pin::run(format, field, &request, target)
        }
        Command::Php(PhpCommand::Upgrade { minor }) => {
            commands::php_upgrade::run(format, field, minor.as_deref())
        }
        Command::Composer(ComposerCommand::Install { request }) => {
            commands::composer_install::run(format, field, request.as_deref())
        }
        Command::Composer(ComposerCommand::Uninstall { request }) => {
            commands::composer_uninstall::run(format, field, &request)
        }
        Command::Composer(ComposerCommand::List) => commands::composer_list::run(format, field),
        Command::Composer(ComposerCommand::Find { request }) => {
            commands::composer_find::run(format, field, request.as_deref())
        }
        Command::Composer(ComposerCommand::Pin { request, toml, composer }) => {
            let target = if toml {
                commands::composer_pin::PinTarget::Toml
            } else if composer {
                commands::composer_pin::PinTarget::Composer
            } else {
                commands::composer_pin::PinTarget::Auto
            };
            commands::composer_pin::run(format, field, &request, target)
        }
        Command::Composer(ComposerCommand::Dir) => commands::composer_dir::run(format, field),
        Command::Composer(ComposerCommand::Upgrade) => {
            commands::composer_upgrade::run(format, field)
        }
        Command::SelfCmd(SelfCommand::Update) => commands::self_update::run(),
        Command::SelfCmd(SelfCommand::Version { short }) => {
            commands::self_version::run(format, field, short)
        }
    }
}
