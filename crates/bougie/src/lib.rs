//! `bougie` binary library: CLI dispatch + the `shim` and `commands`
//! glue that aren't useful outside the bougie executable. Everything
//! else lives in a `bougie-*` workspace crate.

pub mod commands;
pub mod failure;
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
use bougie_cli::{DbCommand, ProjectsCommand, ServiceCommand, ServiceDaemonCommand};
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
    tristate(scripts, no_scripts)
}

/// Fold a `--flag` / `--no-flag` pair into an explicit override: `--flag`
/// wins as `Some(true)`, `--no-flag` as `Some(false)`, neither as `None`
/// (defer to config). clap's `conflicts_with` prevents both being set.
fn tristate(yes: bool, no: bool) -> Option<bool> {
    if yes {
        Some(true)
    } else if no {
        Some(false)
    } else {
        None
    }
}

/// Map the clap-level `--resolution` enum onto the resolver's
/// [`ResolutionStrategy`]. Kept here (rather than as a `From` impl) so
/// `bougie-cli` stays free of a dependency on the resolver crate.
fn resolution_strategy(
    cli: bougie_cli::ResolutionStrategy,
) -> bougie_composer_resolver::ResolutionStrategy {
    use bougie_composer_resolver::ResolutionStrategy as R;
    match cli {
        bougie_cli::ResolutionStrategy::Highest => R::Highest,
        bougie_cli::ResolutionStrategy::Lowest => R::Lowest,
        bougie_cli::ResolutionStrategy::LowestDirect => R::LowestDirect,
    }
}

/// Build a resolve-time platform-ignore filter from the
/// `--ignore-platform-reqs` / `--ignore-platform-req=<req>` flags.
fn platform_ignore(
    ignore_all: bool,
    reqs: &[String],
) -> bougie_composer_resolver::PlatformIgnore {
    bougie_composer_resolver::PlatformIgnore::new(ignore_all, reqs)
}

/// Best-effort verb for a `usage` (parse-failure) telemetry event:
/// the first non-flag argument when it names a known verb, else
/// `"unknown"`. Closed vocabulary in, closed vocabulary out — a
/// typo'd verb never reaches the wire.
pub fn usage_command_name<I>(args: I) -> &'static str
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    args.into_iter()
        .skip(1) // argv[0]
        .filter_map(|a| a.into_string().ok())
        .find(|a| !a.starts_with('-'))
        .and_then(|tok| {
            bougie_telemetry::event::COMMAND_VOCAB.iter().find(|v| **v == tok).copied()
        })
        .unwrap_or("unknown")
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
        Command::Run { .. } => "run",
        Command::Php(_) => "php",
        Command::Node(_) => "node",
        Command::Patches(_) => "patches",
        Command::Composer(_) => "composer",
        Command::Tool(_) => "tool",
        Command::ToolExec { .. } => "tool-exec",
        Command::Cache(_) => "cache",
        Command::SelfCmd(_) => "self",
        Command::Telemetry { .. } => "telemetry",
        Command::TelemetryFlush => "__telemetry-flush",
        Command::Diagnose { .. } => "diagnose",
        Command::Server(_) => "server",
        Command::Share(_) => "share",
        Command::Service(_) => "service",
        Command::Projects(_) => "projects",
        Command::Db(_) => "db",
        Command::Doctor(_) => "doctor",
        Command::Ci(_) => "ci",
        Command::Make { .. } => "make",
        Command::Format { .. } => "format",
        Command::Start { .. } => "start",
        Command::Stop { .. } => "stop",
        Command::Login { .. } => "login",
    }
}

pub fn run(cli: Cli) -> Result<ExitCode> {
    // Progress bars (rendered by `bougie_fetch`) only make sense for an
    // interactive text-mode invocation: a JSON consumer would otherwise
    // see ANSI escapes mixed into stderr alongside the §9.2 event stream,
    // and `--quiet` users opted out of all non-error stderr noise.
    let progress_visible = !cli.quiet
        && matches!(cli.format, OutputFormat::Text)
        && std::io::stderr().is_terminal();
    bougie_output::output::set_progress_visible(progress_visible);
    bougie_output::output::set_verbose(cli.verbose);

    // Top-level span for the whole invocation. Entered on the main
    // thread for the entire dispatch, so a Ctrl-\ activity dump always
    // shows which verb is running even before a deeper phase span opens.
    let command = command_name(&cli.command);
    let _cmd_span = tracing::info_span!("command", name = command).entered();
    // Let a potential crash event name the verb that was running.
    bougie_telemetry::crash::set_command(command);

    // Telemetry wraps dispatch at this single choke point: one
    // `command` event per invocation (duration, outcome category, exit
    // code), appended to the local spool per the consent mode. Init is
    // a no-op when telemetry is off, and recording swallows every
    // failure — telemetry must never fail a command. The telemetry
    // verbs themselves are meta and never recorded (`reset` would
    // re-spool its own event right after purging; the flush child
    // recording itself would self-perpetuate).
    // First-run consent prompt (fallback surface for installs that
    // bypassed the installer consent block: cargo install, docker,
    // Windows irm|iex). Self-gating: only when the mode is undecided,
    // interactive text-mode on a real tty, outside CI and run-shims.
    if !matches!(command, "telemetry" | "__telemetry-flush" | "tool-exec" | "diagnose") {
        bougie_telemetry::prompt::maybe_prompt(
            matches!(cli.format, OutputFormat::Text) && !cli.quiet,
        );
    }

    let recorder = if matches!(command, "telemetry" | "__telemetry-flush") {
        bougie_telemetry::Recorder::disabled()
    } else {
        bougie_telemetry::Recorder::init(
            command,
            bougie_telemetry::BinInfo {
                version: env!("CARGO_PKG_VERSION"),
                build_sha: bougie_cli::BUILD_SHA,
            },
        )
    };
    let started = std::time::Instant::now();
    let result = dispatch(cli);
    let (outcome, exit_code) = match &result {
        // `ExitCode` exposes no getter; a command that *returns* a
        // nonzero code (rather than erroring) records as ok/0. The
        // taxonomy tracks errors, not verb-specific soft-failure codes
        // like `composer audit`'s advisory exit.
        Ok(_) => (bougie_telemetry::OUTCOME_OK, 0),
        Err(err) => (bougie_telemetry::outcome_for_error(err), exit_code_for(err)),
    };
    recorder.record_command(started.elapsed(), outcome, exit_code);
    // With the user's event spooled and their prompt about to return,
    // hand any due upload to a detached, deprioritized child.
    recorder.maybe_spawn_flush();
    result
}

#[allow(clippy::too_many_lines, reason = "top-level command dispatch is one big match")]
fn dispatch(cli: Cli) -> Result<ExitCode> {
    let format = cli.format;
    match cli.command {
        Command::Init { script, toml, name, starter, start } => {
            if let Some(file) = script {
                commands::script::init(format, &file)
            } else {
                commands::init::run(format, toml, name, starter, start)
            }
        }
        Command::New { directory, toml, name, starter, start } => {
            commands::init::run_new(format, &directory, toml, name, starter, start)
        }
        Command::Add {
            packages,
            script,
            dev,
            with_dependencies,
            with_all_dependencies,
            no_sync,
            frozen,
            resolution,
            working_dir,
            dry_run,
            ignore_platform_reqs,
            ignore_platform_req,
        } => {
            if let Some(file) = script {
                commands::script::add(
                    format,
                    &file,
                    &packages,
                    dry_run,
                    resolution_strategy(resolution),
                )
            } else {
                commands::composer_require::add(
                    format,
                    packages,
                    dev,
                    no_sync,
                    frozen,
                    with_dependencies,
                    with_all_dependencies,
                    working_dir,
                    dry_run,
                    resolution_strategy(resolution),
                    platform_ignore(ignore_platform_reqs, &ignore_platform_req),
                )
            }
        }
        Command::Remove {
            packages,
            dev,
            no_sync,
            frozen,
            working_dir,
            dry_run,
            ignore_platform_reqs,
            ignore_platform_req,
        } => commands::composer_require::remove(
            format,
            packages,
            dev,
            frozen,   // --frozen → edit composer.json only (no_update)
            no_sync,  // --no-sync → re-lock but don't install (no_install)
            false,    // top-level remove always considers dev when installing
            working_dir,
            dry_run,
            platform_ignore(ignore_platform_reqs, &ignore_platform_req),
        ),
        Command::Lock { script, resolution, working_dir, dry_run, ignore_platform_reqs, ignore_platform_req } => {
            if let Some(file) = script {
                commands::script::lock(format, &file, dry_run, resolution_strategy(resolution))
            } else {
                commands::lock::run(
                    format,
                    working_dir,
                    dry_run,
                    resolution_strategy(resolution),
                    platform_ignore(ignore_platform_reqs, &ignore_platform_req),
                )
            }
        }
        Command::Tree { package, no_dev, working_dir } => commands::composer_show::run(
            format,
            commands::composer_show::ShowOptions {
                package,
                tree: true,
                dedupe: true,
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
        Command::Sync {
            offline,
            dry_run,
            scripts,
            no_scripts,
            patches,
            no_patches,
            resolution,
            ignore_platform_reqs,
            ignore_platform_req,
            php,
        } => {
            // Walk up to the real project root (uv-parity) so `bougie sync`
            // from a subdirectory syncs the project, not the cwd.
            let project_root = commands::run::resolve_project_root(&std::env::current_dir()?);
            commands::sync::run(
                &project_root,
                format,
                offline,
                dry_run,
                scripts_override(scripts, no_scripts),
                tristate(patches, no_patches),
                php,
                resolution_strategy(resolution),
                platform_ignore(ignore_platform_reqs, &ignore_platform_req),
            )
        }
        Command::Run { script, with, no_sync, xdebug, php_request, php, argv } => {
            if script {
                commands::script::run(&argv, format, php_request.as_deref(), &with, xdebug, php)
            } else {
                commands::run::run(
                    &with,
                    &argv,
                    format,
                    no_sync,
                    xdebug,
                    php,
                    php_request.as_deref(),
                )
            }
        }
        Command::Patches(cmd) => commands::patches_cmd::run(format, cmd),
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
            patches,
            no_patches,
        }) => commands::composer_install::run(
            format,
            working_dir,
            no_dev,
            frozen,
            lock_verify,
            ignore_platform_reqs,
            ignore_platform_req,
            scripts_override(scripts, no_scripts),
            tristate(patches, no_patches),
        ),
        Command::Composer(ComposerCommand::Update {
            packages,
            no_install,
            with_dependencies,
            with_all_dependencies,
            working_dir,
            no_dev,
            resolution,
            prefer_lowest,
            dry_run,
            ignore_platform_reqs,
            ignore_platform_req,
        }) => {
            // Composer's `--prefer-lowest` is the bool twin of uv's
            // `--resolution lowest`; when set it wins over `--resolution`.
            let resolution = if prefer_lowest {
                bougie_cli::ResolutionStrategy::Lowest
            } else {
                resolution
            };
            commands::composer_update::run(
                format,
                working_dir,
                no_dev,
                dry_run,
                no_install,
                packages,
                with_dependencies,
                with_all_dependencies,
                resolution_strategy(resolution),
                platform_ignore(ignore_platform_reqs, &ignore_platform_req),
            )
        }
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
            dev,
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
            dev,
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
            ignore_platform_reqs,
            ignore_platform_req,
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
            platform_ignore(ignore_platform_reqs, &ignore_platform_req),
        ),
        Command::Composer(ComposerCommand::Remove {
            packages,
            dev,
            no_update,
            no_install,
            no_dev,
            ignore_platform_reqs,
            ignore_platform_req,
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
            platform_ignore(ignore_platform_reqs, &ignore_platform_req),
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
                dedupe: false,
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
        Command::Share(args) => commands::share::run(format, args),
        #[cfg(not(unix))]
        Command::Share(_) => unsupported_on_windows("bougie share"),
        #[cfg(unix)]
        Command::Service(ServiceCommand::Up { names, detach }) => {
            commands::service::up::run(format, names, detach)
        }
        #[cfg(unix)]
        Command::Service(ServiceCommand::Down { names, purge }) => {
            commands::service::down::run(format, names, purge)
        }
        #[cfg(unix)]
        Command::Service(ServiceCommand::Add { names }) => {
            commands::service::add::run(format, names)
        }
        #[cfg(unix)]
        Command::Service(ServiceCommand::Remove { names, purge }) => {
            commands::service::remove::run(format, names, purge)
        }
        #[cfg(unix)]
        Command::Service(ServiceCommand::List { all }) => {
            commands::service::list::run(format, all)
        }
        #[cfg(unix)]
        Command::Service(ServiceCommand::Catalog) => commands::service::catalog::run(format),
        #[cfg(unix)]
        Command::Service(ServiceCommand::Exec { service, tool, args }) => {
            commands::service::exec::run(service, tool, args)
        }
        #[cfg(unix)]
        Command::Service(ServiceCommand::Restart { names }) => {
            commands::service::restart::run(format, names)
        }
        #[cfg(unix)]
        Command::Service(ServiceCommand::Status { name }) => {
            commands::service::status::run(format, name)
        }
        #[cfg(unix)]
        Command::Service(ServiceCommand::Credentials { name, env }) => {
            commands::service::credentials::run(format, name, env)
        }
        #[cfg(unix)]
        Command::Service(ServiceCommand::Logs { name, follow, lines }) => {
            commands::service::logs::run(format, name, follow, lines)
        }
        #[cfg(unix)]
        Command::Service(ServiceCommand::Daemon(ServiceDaemonCommand::Status)) => {
            commands::service::daemon::status(format)
        }
        #[cfg(unix)]
        Command::Service(ServiceCommand::Daemon(ServiceDaemonCommand::Stop)) => {
            commands::service::daemon::stop(format)
        }
        #[cfg(unix)]
        Command::Service(ServiceCommand::Daemon(ServiceDaemonCommand::Version)) => {
            commands::service::daemon::version(format)
        }
        #[cfg(not(unix))]
        Command::Service(_) => unsupported_on_windows("bougie service"),
        #[cfg(unix)]
        Command::Projects(ProjectsCommand::List { alloc }) => {
            commands::service::projects::run(format, alloc)
        }
        #[cfg(unix)]
        Command::Projects(ProjectsCommand::Purge { project, all, dry_run, yes }) => {
            commands::service::projects::purge(format, project, all, dry_run, yes)
        }
        #[cfg(not(unix))]
        Command::Projects(_) => unsupported_on_windows("bougie projects"),
        #[cfg(unix)]
        Command::Db(DbCommand::Seed(args)) => commands::db::seed::run(format, args),
        #[cfg(unix)]
        Command::Db(DbCommand::Pull(args)) => commands::db::pull::run(format, args),
        #[cfg(unix)]
        Command::Db(DbCommand::Refresh(args)) => commands::db::refresh::run(format, args),
        #[cfg(unix)]
        Command::Db(DbCommand::Get(args)) => commands::db::get::run(format, args),
        #[cfg(unix)]
        Command::Db(DbCommand::Status(args)) => commands::db::status::run(format, args),
        #[cfg(not(unix))]
        Command::Db(_) => unsupported_on_windows("bougie db"),
        #[cfg(unix)]
        Command::Doctor(args) => commands::doctor::run(format, args),
        #[cfg(not(unix))]
        Command::Doctor(_) => unsupported_on_windows("bougie doctor"),
        #[cfg(unix)]
        Command::Make {
            task,
            list,
            dry_run,
            explain,
            no_sync,
            no_builtin,
            no_team,
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
                no_team,
                recipe,
                print,
            },
        ),
        #[cfg(not(unix))]
        Command::Make { .. } => unsupported_on_windows("bougie make"),
        Command::Login {
            url,
            ci,
            repository,
            audience,
            no_provision,
            composer_json,
        } => {
            let mode = commands::login::ProvisionMode::from_flags(no_provision, composer_json);
            if ci {
                commands::login::run_ci(
                    format,
                    &url,
                    repository.as_deref().unwrap_or_default(),
                    &audience,
                    mode,
                )
            } else {
                commands::login::run(format, &url, mode)
            }
        }
        Command::Format { args } => commands::format::run(&args),
        #[cfg(unix)]
        Command::Ci(bougie_cli::CiCommand::Init(args)) => commands::ci::run(format, args),
        #[cfg(not(unix))]
        Command::Ci(_) => unsupported_on_windows("bougie ci"),
        #[cfg(unix)]
        Command::Start { no_sync, dry_run, explain, no_builtin, recipe } => {
            commands::start::run(
                format,
                commands::start::StartOptions { no_sync, dry_run, explain, no_builtin, recipe },
            )
        }
        #[cfg(not(unix))]
        Command::Start { .. } => unsupported_on_windows("bougie start"),
        // `stop` is the teardown twin of `start`: bring the project's
        // declared services (the dev-server tenant among them) down. A
        // global `server stop` is deliberately *not* run — it would tear
        // down hosting for every other project sharing the daemon.
        #[cfg(unix)]
        Command::Stop { names, purge } => commands::service::down::run(format, names, purge),
        #[cfg(not(unix))]
        Command::Stop { .. } => unsupported_on_windows("bougie stop"),
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
            args.php.as_deref(),
            &args.with,
            args.no_project,
            args.bin.as_deref(),
            args.command,
        ),
        Command::Tool(ToolCommand::Bgx(args)) => commands::tool_run::run(
            format,
            args.tool_run.php.as_deref(),
            &args.tool_run.with,
            args.tool_run.no_project,
            args.tool_run.bin.as_deref(),
            args.tool_run.command,
        ),
        Command::Tool(ToolCommand::List) => commands::tool_list::run(format),
        Command::Tool(ToolCommand::Dir { package }) => {
            commands::tool_dir::run(format, package)
        }
        Command::ToolExec { wrapper, args } => commands::tool_exec::run(&wrapper, args),
        Command::Telemetry { command } => commands::telemetry::run(format, command),
        Command::TelemetryFlush => commands::telemetry_flush::run(),
        Command::Diagnose { issue, yes, edit, no_edit, last, project, args } => {
            commands::diagnose::run(
                format,
                commands::diagnose::DiagnoseArgs { issue, yes, edit, no_edit, last, project, args },
            )
        }
    }
}

#[cfg(test)]
mod usage_name_tests {
    use std::ffi::OsString;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn first_non_flag_token_maps_through_the_vocab() {
        assert_eq!(super::usage_command_name(args(&["bougie", "sync", "--bad"])), "sync");
        assert_eq!(
            super::usage_command_name(args(&["bougie", "--format", "json-v1", "snyc"])),
            "unknown",
        );
        assert_eq!(super::usage_command_name(args(&["bougie", "sylc"])), "unknown");
        assert_eq!(super::usage_command_name(args(&["bougie", "--bad-flag"])), "unknown");
        assert_eq!(super::usage_command_name(args(&["bougie"])), "unknown");
    }
}
