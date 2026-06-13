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
    CacheCommand, ComposerCommand, ExtCommand, NodeCommand, PhpCommand, SelfCommand, ToolCommand,
};
#[cfg(unix)]
use bougie_cli::{ProjectsCommand, ServicesCommand, ServicesDaemonCommand};
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

/// Collapse the `--scripts` / `--no-scripts` flag pair into an explicit
/// override: `Some(true)` for `--scripts`, `Some(false)` for
/// `--no-scripts`, `None` when neither is passed (defer to `[scripts] run`
/// in config). clap's `conflicts_with` guarantees they're not both set.
fn scripts_override(scripts: bool, no_scripts: bool) -> Option<bool> {
    if scripts {
        Some(true)
    } else if no_scripts {
        Some(false)
    } else {
        None
    }
}

/// Stable, human-readable name for the running subcommand, used as the
/// `command` span field so a Ctrl-\ activity dump (and `BOUGIE_LOG`)
/// shows which verb is running even before any deeper span opens.
fn command_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::Init { .. } => "init",
        Command::New { .. } => "new",
        Command::Ext(_) => "ext",
        Command::Add { .. } => "add",
        Command::Remove { .. } => "remove",
        Command::Lock { .. } => "lock",
        Command::Tree { .. } => "tree",
        Command::Outdated { .. } => "outdated",
        Command::Sync { .. } => "sync",
        Command::Up { .. } => "up",
        Command::Down { .. } => "down",
        Command::Run { .. } => "run",
        Command::Php(_) => "php",
        Command::Node(_) => "node",
        Command::Composer(_) => "composer",
        Command::Tool(_) => "tool",
        Command::ToolExec { .. } => "tool-exec",
        Command::Cache(_) => "cache",
        Command::SelfCmd(_) => "self",
        Command::Server(_) => "server",
        Command::Services(_) => "services",
        Command::Projects(_) => "projects",
        Command::Make { .. } => "make",
        Command::Format { .. } => "format",
    }
}

#[allow(clippy::too_many_lines, reason = "top-level command dispatch is one big match")]
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
        Command::Add {
            packages,
            dev,
            with_dependencies,
            with_all_dependencies,
            no_sync,
            frozen,
            working_dir,
            dry_run,
        } => commands::composer_require::add(
            format,
            packages,
            dev,
            no_sync,
            frozen,
            with_dependencies,
            with_all_dependencies,
            working_dir,
            dry_run,
        ),
        Command::Remove {
            packages,
            dev,
            no_sync,
            frozen,
            working_dir,
            dry_run,
        } => commands::composer_require::remove(
            format,
            packages,
            dev,
            frozen,   // --frozen → edit composer.json only (no_update)
            no_sync,  // --no-sync → re-lock but don't install (no_install)
            false,    // top-level remove always considers dev when installing
            working_dir,
            dry_run,
        ),
        Command::Lock { working_dir, dry_run } => commands::lock::run(format, working_dir, dry_run),
        Command::Tree { package, no_dev, working_dir } => commands::composer_show::run(
            format,
            commands::composer_show::ShowOptions {
                package,
                tree: true,
                direct: false,
                platform: false,
                self_: false,
                name_only: false,
                path: false,
                latest: false,
                outdated: false,
                no_dev,
                working_dir,
            },
        ),
        Command::Outdated {
            packages,
            direct,
            major_only,
            minor_only,
            patch_only,
            no_dev,
            strict,
            working_dir,
        } => commands::composer_outdated::run(
            format,
            commands::composer_outdated::OutdatedOptions {
                packages,
                direct,
                major_only,
                minor_only,
                patch_only,
                no_dev,
                strict,
                working_dir,
            },
        ),
        Command::Sync { offline, dry_run, scripts, no_scripts, php } => {
            commands::sync::run(format, offline, dry_run, scripts_override(scripts, no_scripts), php)
        }
        #[cfg(unix)]
        Command::Up { names, detach } => commands::services::up::run(format, names, detach),
        #[cfg(not(unix))]
        Command::Up { names: _, detach: _ } => unsupported_on_windows("bougie up"),
        #[cfg(unix)]
        Command::Down { names, purge } => commands::services::down::run(format, names, purge),
        #[cfg(not(unix))]
        Command::Down { names: _, purge: _ } => unsupported_on_windows("bougie down"),
        Command::Run { with, no_sync, xdebug, php_request, php, argv } => {
            commands::run::run(&with, &argv, format, no_sync, xdebug, php, php_request.as_deref())
        }
        Command::Ext(ExtCommand::Add { args, no_sync, php }) => {
            commands::ext_add_remove::add(format, args, no_sync, php)
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
        Command::Node(NodeCommand::Install { requests }) => {
            commands::node::install(format, &requests)
        }
        Command::Node(NodeCommand::Uninstall { requests }) => {
            commands::node::uninstall(format, &requests)
        }
        Command::Node(NodeCommand::List) => commands::node::list(format),
        Command::Node(NodeCommand::Find { request }) => {
            commands::node::find(format, request.as_deref())
        }
        Command::Node(NodeCommand::Dir) => commands::node::dir(format),
        Command::Composer(ComposerCommand::Install {
            working_dir,
            no_dev,
            frozen,
            lock_verify,
            ignore_platform_reqs,
            ignore_platform_req,
            scripts,
            no_scripts,
        }) => commands::composer_install::run(
            format,
            working_dir,
            no_dev,
            frozen,
            lock_verify,
            ignore_platform_reqs,
            ignore_platform_req,
            scripts_override(scripts, no_scripts),
        ),
        Command::Composer(ComposerCommand::Update {
            packages,
            no_install,
            with_dependencies,
            with_all_dependencies,
            working_dir,
            no_dev,
            dry_run,
            ignore_platform_reqs: _,
            ignore_platform_req: _,
        }) => commands::composer_update::run(
            format,
            working_dir,
            no_dev,
            dry_run,
            no_install,
            packages,
            with_dependencies,
            with_all_dependencies,
        ),
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
        Command::Composer(ComposerCommand::Require {
            packages,
            dev,
            no_update,
            no_install,
            with_dependencies,
            with_all_dependencies,
            prefer_lowest,
            ignore_platform_reqs: _,
            ignore_platform_req: _,
            working_dir,
            dry_run,
        }) => commands::composer_require::require(
            format,
            packages,
            dev,
            no_update,
            no_install,
            with_dependencies,
            with_all_dependencies,
            prefer_lowest,
            working_dir,
            dry_run,
        ),
        Command::Composer(ComposerCommand::Remove {
            packages,
            dev,
            no_update,
            no_install,
            no_dev,
            ignore_platform_reqs: _,
            ignore_platform_req: _,
            working_dir,
            dry_run,
        }) => commands::composer_require::remove(
            format,
            packages,
            dev,
            no_update,
            no_install,
            no_dev,
            working_dir,
            dry_run,
        ),
        Command::Composer(ComposerCommand::Show {
            package,
            tree,
            direct,
            platform,
            self_,
            name_only,
            path,
            latest,
            outdated,
            no_dev,
            working_dir,
        }) => commands::composer_show::run(
            format,
            commands::composer_show::ShowOptions {
                package,
                tree,
                direct,
                platform,
                self_,
                name_only,
                path,
                latest,
                outdated,
                no_dev,
                working_dir,
            },
        ),
        Command::Composer(ComposerCommand::Why {
            package,
            recursive,
            tree,
            working_dir,
        }) => commands::composer_why::why(format, package, recursive, tree, working_dir),
        Command::Composer(ComposerCommand::WhyNot {
            package,
            version,
            recursive,
            tree,
            working_dir,
        }) => commands::composer_why::why_not(format, package, version, recursive, tree, working_dir),
        Command::Composer(ComposerCommand::Outdated {
            packages,
            direct,
            major_only,
            minor_only,
            patch_only,
            no_dev,
            strict,
            working_dir,
        }) => commands::composer_outdated::run(
            format,
            commands::composer_outdated::OutdatedOptions {
                packages,
                direct,
                major_only,
                minor_only,
                patch_only,
                no_dev,
                strict,
                working_dir,
            },
        ),
        Command::Composer(ComposerCommand::Audit {
            no_dev,
            abandoned,
            locked,
            working_dir,
        }) => commands::composer_audit::run(
            format,
            commands::composer_audit::AuditOptions {
                no_dev,
                abandoned,
                locked,
                working_dir,
            },
        ),
        Command::Composer(ComposerCommand::Licenses { no_dev, working_dir }) => {
            commands::composer_licenses::run(format, no_dev, working_dir)
        }
        Command::Composer(ComposerCommand::Status { working_dir }) => {
            commands::composer_status::run(format, working_dir)
        }
        Command::Composer(ComposerCommand::Fund { no_dev, working_dir }) => {
            commands::composer_fund::run(format, no_dev, working_dir)
        }
        Command::Composer(ComposerCommand::External(args)) => {
            let sub = args
                .first()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            Err(eyre::eyre!(
                "`composer {sub}` is not one of bougie's native Composer commands, \
                 and bougie does not bundle the Composer phar.\n\
                 Native commands: install, update, require, remove, show, why, why-not, \
                 outdated, audit, licenses, fund, status, validate, dump-autoload.\n\
                 For the full upstream Composer, install it as a tool:\n    \
                 bougie tool install composer/composer",
            ))
        }
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
        Command::Projects(ProjectsCommand::List { alloc }) => {
            commands::services::projects::run(format, alloc)
        }
        #[cfg(unix)]
        Command::Projects(ProjectsCommand::Purge { project, all, dry_run, yes }) => {
            commands::services::projects::purge(format, project, all, dry_run, yes)
        }
        #[cfg(not(unix))]
        Command::Projects(_) => unsupported_on_windows("bougie projects"),
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
        Command::Format { args } => commands::format::run(&args),
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
        Command::Tool(ToolCommand::Run(args)) => {
            commands::tool_run::run(format, args.php.as_deref(), &args.with, args.command)
        }
        Command::Tool(ToolCommand::Bgx(args)) => commands::tool_run::run(
            format,
            args.tool_run.php.as_deref(),
            &args.tool_run.with,
            args.tool_run.command,
        ),
        Command::Tool(ToolCommand::List) => commands::tool_list::run(format),
        Command::Tool(ToolCommand::Dir { package }) => {
            commands::tool_dir::run(format, package)
        }
        Command::ToolExec { wrapper, args } => commands::tool_exec::run(&wrapper, args),
    }
}
