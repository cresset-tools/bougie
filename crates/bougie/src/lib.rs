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

use bougie_cli::{
    CacheCommand, ComposerCommand, ExtCommand, PhpCommand, SelfCommand, ToolCommand,
};
#[cfg(unix)]
use bougie_cli::{ServicesCommand, ServicesDaemonCommand};
use eyre::Result;
use std::io::IsTerminal;
use std::process::ExitCode;

#[cfg(not(unix))]
fn unsupported_on_windows(feature: &str) -> Result<ExitCode> {
    Err(eyre::eyre!(
        "{feature} is not supported on Windows yet — see SERVICES.md / SERVER.md.\n\
         hint:    bougie's daemon and server features require Unix domain sockets and POSIX\n\
                  signals; the CLI subset (php, composer, sync, run, ext, cache, self) works."
    ))
}

/// Stable, human-readable name for the running subcommand, used as the
/// `command` span field so a Ctrl-\ activity dump (and `BOUGIE_LOG`)
/// shows which verb is running even before any deeper span opens.
fn command_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::Init { .. } => "init",
        Command::New { .. } => "new",
        Command::Ext(_) => "ext",
        Command::Sync { .. } => "sync",
        Command::Up { .. } => "up",
        Command::Down { .. } => "down",
        Command::Run { .. } => "run",
        Command::Php(_) => "php",
        Command::Composer(_) => "composer",
        Command::Tool(_) => "tool",
        Command::ToolExec { .. } => "tool-exec",
        Command::Cache(_) => "cache",
        Command::SelfCmd(_) => "self",
        Command::Server(_) => "server",
        Command::Services(_) => "services",
        Command::Make { .. } => "make",
    }
}

pub fn run(cli: Cli) -> Result<ExitCode> {
    let format = cli.format;

    // Progress bars (rendered by `bougie_fetch`) only make sense for an
    // interactive text-mode invocation: a JSON consumer would otherwise
    // see ANSI escapes mixed into stderr alongside the §9.2 event stream,
    // and `--quiet` users opted out of all non-error stderr noise.
    let progress_visible = !cli.quiet
        && matches!(format, OutputFormat::Text)
        && std::io::stderr().is_terminal();
    bougie_output::output::set_progress_visible(progress_visible);
    bougie_output::output::set_verbose(cli.verbose);

    // Top-level span for the whole invocation. Entered on the main
    // thread for the entire dispatch, so a Ctrl-\ activity dump always
    // shows which verb is running even before a deeper phase span opens.
    let _cmd_span = tracing::info_span!("command", name = command_name(&cli.command)).entered();

    match cli.command {
        Command::Init { toml, name, starter, start } => {
            commands::init::run(format, toml, name, starter, start)
        }
        Command::New { directory, toml, name, starter, start } => {
            commands::init::run_new(format, &directory, toml, name, starter, start)
        }
        Command::Sync { offline, dry_run } => commands::sync::run(format, offline, dry_run),
        #[cfg(unix)]
        Command::Up { names, detach } => commands::services::up::run(format, names, detach),
        #[cfg(not(unix))]
        Command::Up { names: _, detach: _ } => unsupported_on_windows("bougie up"),
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
            ignore_platform_reqs,
            ignore_platform_req,
        }) => commands::composer_install::run(
            format,
            working_dir,
            no_dev,
            frozen,
            lock_verify,
            ignore_platform_reqs,
            ignore_platform_req,
        ),
        Command::Composer(ComposerCommand::Update {
            working_dir,
            no_dev,
            dry_run,
            ignore_platform_reqs: _,
            ignore_platform_req: _,
        }) => commands::composer_update::run(format, working_dir, no_dev, dry_run),
        Command::Composer(ComposerCommand::Validate {
            working_dir,
            strict,
            no_check_lock,
            no_check_publish,
            no_check_all,
            with_dependencies,
            check_lock: _,
        }) => commands::composer_validate::run(
            format,
            working_dir,
            commands::composer_validate::ValidateOptions {
                strict,
                no_check_lock,
                no_check_publish,
                no_check_all,
                with_dependencies,
            },
        ),
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
        Command::Composer(ComposerCommand::External(args)) => shim::run_composer(args),
        Command::SelfCmd(SelfCommand::Update { force }) => commands::self_update::run(force),
        Command::SelfCmd(SelfCommand::Version { short }) => {
            commands::self_version::run(format, short)
        }
        Command::Server(args) => commands::server::dispatch(format, args),
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
        Command::Services(ServicesCommand::Projects { action, alloc }) => match action {
            None => commands::services::projects::run(format, alloc),
            Some(bougie_cli::ProjectsAction::Purge { project, all, dry_run, yes }) => {
                commands::services::projects::purge(format, project, all, dry_run, yes)
            }
        },
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
        Command::Tool(ToolCommand::Install { package, php, with, force }) => {
            commands::tool_install::run(format, &package, php.as_deref(), &with, force)
        }
        Command::Tool(ToolCommand::Uninstall { package }) => {
            commands::tool_uninstall::run(format, &package)
        }
        Command::Tool(ToolCommand::Inject { package, with }) => {
            commands::tool_inject::run(format, &package, &with)
        }
        Command::Tool(ToolCommand::Uninject { package, with }) => {
            commands::tool_uninject::run(format, &package, &with)
        }
        Command::Tool(ToolCommand::Upgrade { package, all, reinstall }) => {
            commands::tool_upgrade::run(format, package.as_deref(), all, reinstall)
        }
        Command::Tool(ToolCommand::Run(args)) => commands::tool_run::run(
            format,
            &args.package,
            args.php.as_deref(),
            &args.with,
            args.args,
        ),
        Command::Tool(ToolCommand::Bgx(args)) => commands::tool_run::run(
            format,
            &args.tool_run.package,
            args.tool_run.php.as_deref(),
            &args.tool_run.with,
            args.tool_run.args,
        ),
        Command::Tool(ToolCommand::List) => commands::tool_list::run(format),
        Command::Tool(ToolCommand::Dir { package }) => {
            commands::tool_dir::run(format, package)
        }
        Command::ToolExec { wrapper, args } => commands::tool_exec::run(&wrapper, args),
    }
}
