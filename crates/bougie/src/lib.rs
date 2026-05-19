//! `bougie` binary library: CLI dispatch + the `shim` and `commands`
//! glue that aren't useful outside the bougie executable. Everything
//! else lives in a `bougie-*` workspace crate.

pub mod commands;
pub mod shim;

// Re-exports kept so `main.rs` and integration tests can write
// `bougie::{Cli, exit_code_for, shim, Paths, Triple}` without
// caring which leaf crate hosts them.
pub use bougie_cli::{Cli, Command, OutputFormat};
pub use bougie_errors::{exit_code_for, BougieError};
pub use bougie_paths::Paths;
pub use bougie_platform::target::Triple;

use bougie_cli::{CacheCommand, ComposerCommand, ExtCommand, PhpCommand, SelfCommand, ServerCommand};
#[cfg(unix)]
use bougie_cli::{
    ServerHostsCommand, ServerTlsCommand, ServicesCommand, ServicesDaemonCommand,
};
use eyre::Result;
use std::process::ExitCode;

#[cfg(not(unix))]
fn unsupported_on_windows(feature: &str) -> Result<ExitCode> {
    Err(eyre::eyre!(
        "{feature} is not supported on Windows yet — see SERVICES.md / SERVER.md.\n\
         hint:    bougie's daemon and server features require Unix domain sockets and POSIX\n\
                  signals; the CLI subset (php, composer, sync, run, ext, cache, self) works."
    ))
}

pub fn run(cli: Cli) -> Result<ExitCode> {
    let format = cli.format;

    // Progress bars (rendered by `bougie_fetch`) only make sense for an
    // interactive text-mode invocation: a JSON consumer would otherwise
    // see ANSI escapes mixed into stderr alongside the §9.2 event stream,
    // and `--quiet` users opted out of all non-error stderr noise.
    use std::io::IsTerminal;
    let progress_visible = !cli.quiet
        && matches!(format, OutputFormat::Text)
        && std::io::stderr().is_terminal();
    bougie_output::output::set_progress_visible(progress_visible);

    match cli.command {
        Command::Init { toml } => commands::init::run(format, toml),
        Command::Sync { offline: _, dry_run } => commands::sync::run(format, dry_run),
        #[cfg(unix)]
        Command::Up { names } => commands::services::up::run(format, names),
        #[cfg(not(unix))]
        Command::Up { names: _ } => unsupported_on_windows("bougie up"),
        #[cfg(unix)]
        Command::Down { names, purge } => commands::services::down::run(format, names, purge),
        #[cfg(not(unix))]
        Command::Down { names: _, purge: _ } => unsupported_on_windows("bougie down"),
        Command::Run { with, no_sync, xdebug, argv } => {
            commands::run::run(&with, &argv, format, no_sync, xdebug)
        }
        Command::Ext(ExtCommand::Add { args, no_sync }) => {
            commands::ext_add_remove::add(format, args, no_sync)
        }
        Command::Ext(ExtCommand::Remove { names, no_sync }) => {
            commands::ext_add_remove::remove(format, names, no_sync)
        }
        Command::Ext(ExtCommand::List {
            only_installed,
            only_available,
            all_versions,
            all_platforms,
            show_urls,
        }) => commands::ext_list::run(
            format,
            commands::ext_list::Options {
                only_installed,
                only_available,
                all_versions,
                all_platforms,
                show_urls,
            },
        ),
        Command::Cache(CacheCommand::Dir) => commands::cache_dir::run(format),
        Command::Cache(CacheCommand::Clean) => commands::cache_clean::run(format),
        Command::Cache(CacheCommand::Size) => commands::cache_size::run(format),
        Command::Cache(CacheCommand::Prune { dry_run, prune_projects: _ }) => {
            commands::cache_prune::run(format, dry_run)
        }
        Command::Php(PhpCommand::Dir) => commands::php_dir::run(format),
        Command::Php(PhpCommand::Install {
            requests,
            flavor,
            bare,
            without,
        }) => commands::php_install::run(
            format,
            &requests,
            flavor.as_deref(),
            bare,
            &without,
        ),
        Command::Php(PhpCommand::Uninstall { requests, flavor }) => commands::php_uninstall::run(
            format,
            &requests,
            flavor.as_deref(),
        ),
        Command::Php(PhpCommand::List {
            request,
            only_installed,
            only_available,
            all_versions,
            all_platforms,
            all_arches,
            show_urls,
        }) => commands::php_list::run(
            format,
            commands::php_list::Options {
                request: request.as_deref(),
                only_installed,
                only_available,
                all_versions,
                all_platforms,
                all_arches,
                show_urls,
            },
        ),
        Command::Php(PhpCommand::Find { request }) => {
            commands::php_find::run(format, request.as_deref())
        }
        Command::Php(PhpCommand::Pin { request, toml, composer }) => {
            let target = if toml {
                commands::php_pin::PinTarget::Toml
            } else if composer {
                commands::php_pin::PinTarget::Composer
            } else {
                commands::php_pin::PinTarget::Auto
            };
            commands::php_pin::run(format, &request, target)
        }
        Command::Php(PhpCommand::Upgrade { minor }) => {
            commands::php_upgrade::run(format, minor.as_deref())
        }
        Command::Composer(ComposerCommand::Install {
            working_dir,
            no_dev,
            frozen,
            lock_verify,
        }) => commands::composer_install::run(format, working_dir, no_dev, frozen, lock_verify),
        Command::Composer(ComposerCommand::Fetch { request }) => {
            commands::composer_fetch::run(format, request.as_deref())
        }
        Command::Composer(ComposerCommand::Uninstall { request }) => {
            commands::composer_uninstall::run(format, &request)
        }
        Command::Composer(ComposerCommand::List) => commands::composer_list::run(format),
        Command::Composer(ComposerCommand::Find { request }) => {
            commands::composer_find::run(format, request.as_deref())
        }
        Command::Composer(ComposerCommand::Pin { request, toml, composer }) => {
            let target = if toml {
                commands::composer_pin::PinTarget::Toml
            } else if composer {
                commands::composer_pin::PinTarget::Composer
            } else {
                commands::composer_pin::PinTarget::Auto
            };
            commands::composer_pin::run(format, &request, target)
        }
        Command::Composer(ComposerCommand::Dir) => commands::composer_dir::run(format),
        Command::Composer(ComposerCommand::Upgrade) => commands::composer_upgrade::run(format),
        Command::Composer(ComposerCommand::DumpAutoloader {
            optimize,
            classmap_authoritative,
            no_dev,
            apcu_autoloader,
            apcu_prefix,
            autoloader_suffix,
            working_dir,
        }) => commands::composer_dump_autoloader::run(
            format,
            working_dir,
            optimize,
            classmap_authoritative,
            no_dev,
            apcu_autoloader,
            apcu_prefix,
            autoloader_suffix,
        ),
        Command::SelfCmd(SelfCommand::Update) => commands::self_update::run(),
        Command::SelfCmd(SelfCommand::Version { short }) => {
            commands::self_version::run(format, short)
        }
        Command::Server(ServerCommand::Run { config, listen, log_format }) => {
            bougie_server::server::run::run(
                format,
                &config,
                listen.as_deref(),
                log_format.as_deref(),
            )
        }
        Command::Server(ServerCommand::List { config }) => {
            bougie_server::server::helpers::list(format, &config)
        }
        #[cfg(unix)]
        Command::Server(ServerCommand::Hosts(ServerHostsCommand::Apply { config })) => {
            bougie_server::server::hosts::apply(format, &config)
        }
        #[cfg(unix)]
        Command::Server(ServerCommand::Tls(ServerTlsCommand::Install)) => {
            bougie_server::server::tls::install(format)
        }
        #[cfg(unix)]
        Command::Server(ServerCommand::Tls(ServerTlsCommand::Uninstall)) => {
            bougie_server::server::tls::uninstall(format)
        }
        #[cfg(not(unix))]
        Command::Server(ServerCommand::Hosts(_) | ServerCommand::Tls(_)) => {
            unsupported_on_windows("bougie server hosts/tls")
        }
        #[cfg(unix)]
        Command::Services(ServicesCommand::Add { names }) => {
            commands::services::add::run(format, names)
        }
        #[cfg(unix)]
        Command::Services(ServicesCommand::Remove { names, purge }) => {
            commands::services::remove::run(format, names, purge)
        }
        #[cfg(unix)]
        Command::Services(ServicesCommand::List { all }) => {
            commands::services::list::run(format, all)
        }
        #[cfg(unix)]
        Command::Services(ServicesCommand::Catalog) => commands::services::catalog::run(format),
        #[cfg(unix)]
        Command::Services(ServicesCommand::Restart { names }) => {
            commands::services::restart::run(format, names)
        }
        #[cfg(unix)]
        Command::Services(ServicesCommand::Status { name }) => {
            commands::services::status::run(format, name)
        }
        #[cfg(unix)]
        Command::Services(ServicesCommand::Logs { name, follow, lines }) => {
            commands::services::logs::run(format, name, follow, lines)
        }
        #[cfg(unix)]
        Command::Services(ServicesCommand::Daemon(ServicesDaemonCommand::Status)) => {
            commands::services::daemon::status(format)
        }
        #[cfg(unix)]
        Command::Services(ServicesCommand::Daemon(ServicesDaemonCommand::Stop)) => {
            commands::services::daemon::stop(format)
        }
        #[cfg(unix)]
        Command::Services(ServicesCommand::Daemon(ServicesDaemonCommand::Version)) => {
            commands::services::daemon::version(format)
        }
        #[cfg(not(unix))]
        Command::Services(_) => unsupported_on_windows("bougie services"),
        #[cfg(unix)]
        Command::Make {
            task,
            list,
            dry_run,
            explain,
            no_sync,
            no_builtin,
            recipe,
            print,
        } => commands::make::run(
            format,
            commands::make::MakeOptions {
                task,
                list,
                dry_run,
                explain,
                no_sync,
                no_builtin,
                recipe,
                print,
            },
        ),
        #[cfg(not(unix))]
        Command::Make { .. } => unsupported_on_windows("bougie make"),
    }
}
