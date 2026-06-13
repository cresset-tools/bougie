use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, Parser, Subcommand};
use std::ffi::OsString;

/// Full version string, uv-style: `0.6.4 (63c5f57d3 2026-05-08 <target>)`.
///
/// Built by `build.rs`; degrades to the bare crate version when git metadata is
/// unavailable. clap prefixes the binary name, so `--version` prints
/// `bougie 0.6.4 (...)`.
pub const LONG_VERSION: &str = env!("BOUGIE_LONG_VERSION");

const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Blue.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Magenta.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::BrightMagenta.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::BrightMagenta.on_default())
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
    .valid(AnsiColor::Green.on_default())
    .invalid(AnsiColor::Yellow.on_default().effects(Effects::BOLD));

#[derive(Parser, Debug)]
#[command(name = "bougie", version = LONG_VERSION, about, long_about = None, styles = HELP_STYLES)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Suppress non-error output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Verbose output.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Output format.
    #[arg(long, global = true, default_value = "text")]
    pub format: OutputFormat,
}

/// Shared PHP-source preference flags (uv's system-Python model adapted
/// to PHP). Flattened into `sync` / `run`; `--managed-php` and
/// `--no-managed-php` are mutually exclusive. With none set, bougie's
/// default applies: prefer an installed managed PHP, then a qualifying
/// system PHP, then download a managed one.
#[derive(Args, Debug, Clone, Copy, Default)]
pub struct PhpPrefArgs {
    /// Only use a bougie-managed PHP; never a system PHP.
    #[arg(long, conflicts_with = "no_managed_php")]
    pub managed_php: bool,
    /// Only use a system PHP already on this machine; never a managed one.
    #[arg(long)]
    pub no_managed_php: bool,
    /// Never download a managed PHP — use an installed managed PHP or a
    /// system one. Errors if neither is present.
    #[arg(long)]
    pub no_php_downloads: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    /// `json-v1` is bougie's structured envelope; `json` is accepted as
    /// an alias so Composer-compatible subcommands (`composer show
    /// --format json`, etc.) work with the same global flag.
    #[value(name = "json-v1", alias = "json")]
    JsonV1,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a new project.
    Init {
        /// Place bougie configuration in a bougie.toml file.
        #[arg(long)]
        toml: bool,
        /// Set the package name (`vendor/package`) of the generated
        /// composer.json. Overrides the name from a `--starter` manifest.
        #[arg(long, value_name = "VENDOR/PACKAGE")]
        name: Option<String>,
        /// Scaffold from a starter pack: a built-in alias (e.g. `mageos`)
        /// or an https URL serving a starter manifest. Writes the
        /// starter's composer.json instead of the empty default.
        #[arg(long, value_name = "URL_OR_ALIAS")]
        starter: Option<String>,
        /// After scaffolding, bring the project up — equivalent to
        /// `bougie start` (sync the toolchain + vendor, then run the
        /// project recipe). Unix-only.
        #[arg(long)]
        start: bool,
    },

    /// Create a new project in a new directory.
    New {
        /// Directory to create under the current directory and scaffold
        /// the project into.
        #[arg(value_name = "DIRECTORY")]
        directory: String,
        /// Place bougie configuration in a bougie.toml file.
        #[arg(long)]
        toml: bool,
        /// Set the package name (`vendor/package`) of the generated
        /// composer.json. Overrides the name from a `--starter` manifest.
        #[arg(long, value_name = "VENDOR/PACKAGE")]
        name: Option<String>,
        /// Scaffold from a starter pack: a built-in alias (e.g. `mageos`)
        /// or an https URL serving a starter manifest.
        #[arg(long, value_name = "URL_OR_ALIAS")]
        starter: Option<String>,
        /// After scaffolding, bring the project up — equivalent to
        /// `bougie start`. Unix-only.
        #[arg(long)]
        start: bool,
    },

    /// Manage PHP extensions.
    #[command(subcommand)]
    Ext(ExtCommand),

    /// Add one or more packages to the project and sync. The uv-flavored
    /// twin of `composer require`: a bare `vendor/pkg` writes a `>=X.Y`
    /// lower bound (vs `composer require`'s caret `^X.Y`), and an
    /// explicit constraint uses the `@` syntax (`vendor/pkg@^1.0`), as in
    /// `bougie tool install` / `bougie ext add`. Edits `composer.json`,
    /// re-resolves `composer.lock`, and installs into `vendor/`.
    Add {
        /// Packages to add, `vendor/pkg` or `vendor/pkg@<constraint>`.
        #[arg(value_name = "PACKAGES", required = true)]
        packages: Vec<String>,
        /// Add to `require-dev` instead of `require`.
        #[arg(long = "dev")]
        dev: bool,
        /// Also update the new packages' dependencies (`-w`).
        #[arg(short = 'w', long = "with-dependencies")]
        with_dependencies: bool,
        /// Also update all dependencies, including shared ones (`-W`).
        #[arg(short = 'W', long = "with-all-dependencies")]
        with_all_dependencies: bool,
        /// Update `composer.json` + `composer.lock` but don't install
        /// into `vendor/`.
        #[arg(long = "no-sync")]
        no_sync: bool,
        /// Edit `composer.json` only — don't touch the lock or `vendor/`.
        #[arg(long = "frozen")]
        frozen: bool,
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Resolve and report what would change without writing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },

    /// Remove one or more packages from the project and sync. The
    /// uv-flavored twin of `composer remove`.
    Remove {
        /// Packages to remove (`vendor/name`).
        #[arg(value_name = "PACKAGES", required = true)]
        packages: Vec<String>,
        /// Remove from `require-dev` instead of `require`.
        #[arg(long = "dev")]
        dev: bool,
        /// Re-resolve `composer.lock` but don't touch `vendor/`.
        #[arg(long = "no-sync")]
        no_sync: bool,
        /// Edit `composer.json` only — don't touch the lock or `vendor/`.
        #[arg(long = "frozen")]
        frozen: bool,
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Resolve and report what would change without writing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },

    /// Refresh `composer.lock` to match `composer.json` (native; uv's
    /// `uv lock`). Minimal: keeps every package at its locked version
    /// where still valid, re-resolving only what changed. Never bumps
    /// versions and never installs — use `bougie composer update` to pull
    /// newer versions.
    Lock {
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Resolve and report what would change without writing the lock.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },

    /// Print the project's dependency tree (native; uv's `uv tree`).
    /// Reads `composer.lock`.
    Tree {
        /// Root the tree at this package instead of the project.
        #[arg(value_name = "PACKAGE")]
        package: Option<String>,
        /// Skip dev dependencies.
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },

    /// List installed packages with a newer version available (native;
    /// like `uv`/`pnpm outdated`). Reads `composer.lock` and queries the
    /// configured repositories.
    Outdated {
        /// Optional `vendor/name` filters; with none, all are considered.
        #[arg(value_name = "PACKAGES")]
        packages: Vec<String>,
        /// Only the project's direct dependencies (`--direct` / `-D`).
        #[arg(short = 'D', long = "direct")]
        direct: bool,
        /// Only packages with a new major version.
        #[arg(long = "major-only")]
        major_only: bool,
        /// Only packages with a new minor version.
        #[arg(long = "minor-only")]
        minor_only: bool,
        /// Only packages with a new patch version.
        #[arg(long = "patch-only")]
        patch_only: bool,
        /// Skip dev dependencies.
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Exit non-zero if any package is outdated.
        #[arg(long = "strict")]
        strict: bool,
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },

    /// Install everything the project requires.
    Sync {
        /// Don't try to download anything, this will fail if there are uncached packages.
        #[arg(long)]
        offline: bool,
        /// Show the plan, change nothing on disk.
        #[arg(long)]
        dry_run: bool,
        /// Run composer.json root scripts for this sync, overriding
        /// `[scripts] run` in bougie.toml. Off by default (opt-in).
        #[arg(long, conflicts_with = "no_scripts")]
        scripts: bool,
        /// Skip composer.json root scripts for this sync, overriding
        /// `[scripts] run = true` in bougie.toml.
        #[arg(long = "no-scripts")]
        no_scripts: bool,
        #[command(flatten)]
        php: PhpPrefArgs,
    },

    /// Start the project's declared services (or every service in
    /// `names`) and provision the project's tenant in each. Equivalent
    /// to the former `bougie services up` — promoted to a top-level
    /// verb because it's the most common project-startup step.
    Up {
        /// Service names to bring up. Empty = every declared service.
        names: Vec<String>,
        /// Start the services and return immediately instead of
        /// attaching to their combined log stream. Attaching is the
        /// default for an interactive (TTY) text-mode invocation;
        /// non-interactive runs and `--format json-v1` always detach.
        #[arg(short = 'd', long)]
        detach: bool,
    },

    /// Stop the project's declared services (or every service in
    /// `names`). The shared global process stays up while any other
    /// project's tenant remains. Equivalent to the former
    /// `bougie services down`.
    Down {
        names: Vec<String>,
        /// Destroy persisted tenant data (e.g. FLUSHDB on redis). Off
        /// by default — re-adding the service should restore state.
        #[arg(long)]
        purge: bool,
    },

    /// Run a command in the project environment.
    Run {
        /// Add a temporary extension for this invocation.
        #[arg(long, value_name = "EXT=VER")]
        with: Vec<String>,
        /// Skip the implicit `bougie sync` before running.
        #[arg(long)]
        no_sync: bool,
        /// Layer the server's debug overlay (`.bougie/conf.d-debug/`)
        /// into `PHP_INI_SCAN_DIR` and set `XDEBUG_SESSION=1` for the
        /// child. Installs xdebug on first use if not already present.
        #[arg(long)]
        xdebug: bool,
        /// Run with a specific PHP interpreter. Accepts a version
        /// (`8.3`, `8.3.12`), a constraint (`~8.3`, `>=8.2,<8.4`), or a
        /// path to a `php` binary. Forces a sync to that interpreter,
        /// so it can't be combined with `--no-sync`. Mirrors
        /// `uv run --python`.
        #[arg(long = "php", value_name = "VER|PATH", conflicts_with = "no_sync")]
        php_request: Option<String>,
        #[command(flatten)]
        php: PhpPrefArgs,
        /// Command and arguments. `--` separator is optional.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        argv: Vec<String>,
    },

    /// Manage PHP interpreters.
    #[command(subcommand)]
    Php(PhpCommand),

    /// Run Composer, reimplemented natively. bougie does not bundle or
    /// execute the Composer phar; the common Composer surface
    /// (install/update/require/remove/show/why/why-not/outdated/audit/
    /// licenses/fund/status/validate/dump-autoload) runs natively, and an
    /// unrecognized subcommand errors with a pointer to
    /// `bougie tool install composer/composer` for the full upstream
    /// Composer.
    #[command(subcommand)]
    Composer(ComposerCommand),

    /// Manage globally-installed, isolated PHP CLI tools. See
    /// `TOOL_PLAN.md` for the design.
    #[command(subcommand)]
    Tool(ToolCommand),

    /// Runtime shim invoked by tool wrappers (`#!.../bougie tool-exec`).
    /// Not for direct CLI use; hidden from `--help`.
    #[command(hide = true, name = "tool-exec")]
    ToolExec {
        /// Path to the tool wrapper script the kernel handed us as
        /// argv[1] via the shebang.
        wrapper: std::path::PathBuf,
        /// User-supplied arguments to the tool, passed through to PHP.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },

    /// Manage bougie's cache.
    #[command(subcommand)]
    Cache(CacheCommand),

    /// Manage the bougie binary itself.
    #[command(subcommand)]
    #[command(name = "self")]
    SelfCmd(SelfCommand),

    /// Run the bougie development HTTP server for the current project.
    /// With no subcommand, registers the project with the shared dev
    /// server, prints its URL, and streams its log (Ctrl-C detaches).
    /// See SERVER.md.
    Server(ServerArgs),

    /// Manage project-scoped dev services (mariadb, redis, …). See
    /// SERVICES.md and CLI.md §3.8.
    #[command(subcommand)]
    Services(ServicesCommand),

    /// Walk a project recipe's DAG, running tasks whose freshness
    /// check fails. `bougie start` is a zero-arg alias for
    /// `bougie make start`. See RECIPES.md.
    #[command(alias = "start")]
    Make {
        /// Task to run. Defaults to `start` — so `bougie make` and
        /// `bougie start` are equivalent.
        task: Option<String>,
        /// List available tasks instead of running.
        #[arg(long, conflicts_with_all = ["dry_run", "explain", "print"])]
        list: bool,
        /// Show what would run, but don't execute.
        #[arg(long)]
        dry_run: bool,
        /// Explain why each step runs or skips.
        #[arg(long)]
        explain: bool,
        /// Skip the implicit `bougie sync` prologue.
        #[arg(long)]
        no_sync: bool,
        /// Ignore the builtin recipe; use only `bougie.toml`.
        #[arg(long)]
        no_builtin: bool,
        /// Force a specific builtin (e.g. `magento`).
        #[arg(long, value_name = "NAME")]
        recipe: Option<String>,
        /// Print the merged recipe to stdout instead of running.
        #[arg(long)]
        print: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServicesCommand {
    /// Declare a service in the project. Errors if the name isn't in
    /// the catalog. Use `bougie services catalog` to discover names.
    Add {
        /// One or more service names, each optionally `@<version>`.
        names: Vec<String>,
    },
    /// Remove a service declaration from the project.
    Remove {
        /// Service names to remove. `--purge` is reserved for the
        /// future tenant-data-destruction path; today it has no effect.
        names: Vec<String>,
        /// Reserved — see CLI.md §3.8.2. Today this only echoes back.
        #[arg(long)]
        purge: bool,
    },
    /// List the services declared in the current project.
    List {
        /// Reserved for cross-project listing in Phase 3+. Today this
        /// degrades silently to per-project output.
        #[arg(long)]
        all: bool,
    },
    /// List every provisioned tenant across the shared services and the
    /// project each belongs to. Reads the on-disk tenant ledgers; no
    /// daemon required. With `purge`, deprovisions tenants instead.
    Projects {
        #[command(subcommand)]
        action: Option<ProjectsAction>,
        /// Show the per-service allocation (redis db number, rabbitmq
        /// vhost, server hostname, …) as an extra column.
        #[arg(long)]
        alloc: bool,
    },
    /// Print the built-in service catalog (no daemon required).
    Catalog,
    /// Restart the named services (or every declared service). Stops
    /// then starts the underlying global process; the tenant ledger
    /// is preserved, so generated passwords / DB numbers survive.
    /// Affects every project sharing the same service.
    Restart {
        names: Vec<String>,
    },
    /// Per-service status for the current project.
    Status {
        /// Limit to a single service.
        name: Option<String>,
    },
    /// Tail (and optionally follow) service logs. With no name, shows
    /// the combined ("multilog") stream of every service declared in the
    /// project, each line prefixed with its (colorized) service name —
    /// the same view `bougie up` attaches to.
    Logs {
        /// Service name. Omit to tail every declared service at once.
        name: Option<String>,
        /// Follow the log; runs until interrupted (Ctrl-C).
        #[arg(short = 'f', long)]
        follow: bool,
        /// Number of trailing lines to print before any follow.
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    /// Inspect and control the `bougied` daemon.
    #[command(subcommand)]
    Daemon(ServicesDaemonCommand),
}

#[derive(Subcommand, Debug)]
pub enum ProjectsAction {
    /// Deprovision tenants and remove them from the service ledgers.
    /// With no flags, targets *orphaned* tenants whose project directory
    /// no longer exists. Destructive: when the service is running this
    /// drops the tenant's data (database, vhost, redis db, …); when it's
    /// stopped, only the ledger entry is removed.
    Purge {
        /// Purge a specific project's tenants by path (it may already be
        /// deleted) instead of the orphaned set.
        #[arg(long)]
        project: Option<String>,
        /// Purge every tenant of every project. Use with care.
        #[arg(long)]
        all: bool,
        /// Print what would be purged and exit without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt (required for non-interactive use).
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServicesDaemonCommand {
    /// Print daemon PID, socket path, and managed-service count. The
    /// daemon is auto-spawned if not already running.
    Status,
    /// Send a graceful shutdown to the running daemon.
    Stop,
    /// Print the daemon's reported version (used by the CLI to detect
    /// post-`self update` daemon-binary mismatches).
    Version,
}

/// `bougie server` — the project verb plus its management subcommands.
/// With no subcommand, the flattened [`ServeArgs`] drive the default
/// "serve the current project" action; otherwise a [`ServerCommand`]
/// runs.
#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct ServerArgs {
    #[command(subcommand)]
    pub command: Option<ServerCommand>,
    #[command(flatten)]
    pub serve: ServeArgs,
}

/// Default-action arguments for `bougie server` (no subcommand):
/// register the current project with the shared dev server, print its
/// URL, and stream its log.
#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)] // each bool is a distinct CLI flag
pub struct ServeArgs {
    /// Hostname label override — the `<name>` in `<name>.bougie.run`.
    /// Defaults to a name derived from the project.
    #[arg(value_name = "NAME")]
    pub name: Option<String>,
    /// Open the project URL in a browser once the server is ready.
    #[arg(long)]
    pub open: bool,
    /// Serve over HTTPS (requires `bougie server tls install`).
    #[arg(long)]
    pub tls: bool,
    /// Print the URL and return instead of attaching to the log stream.
    #[arg(long)]
    pub no_attach: bool,
    /// Skip the implicit `bougie sync` before serving.
    #[arg(long)]
    pub no_sync: bool,
}

#[derive(Subcommand, Debug)]
pub enum ServerCommand {
    /// Low-level primitive: run the server process against an explicit
    /// multi-host `server.toml`, foreground, with no daemon. This is
    /// what `bougied` spawns and what CI / power users invoke directly;
    /// `--config` is required because a multi-host server has no single
    /// project to default to. The bougied-managed path (`bougie up
    /// server`) supplies its own service-scoped `server.toml`.
    Run {
        /// `server.toml` path. Required.
        #[arg(long, value_name = "PATH")]
        config: std::path::PathBuf,
        /// CLI override of `[server].listen` (e.g. `127.0.0.1:7080`).
        #[arg(long, value_name = "ADDR")]
        listen: Option<String>,
        /// CLI override of `[server].log_format`.
        #[arg(long, value_name = "FMT")]
        log_format: Option<String>,
    },
    /// Show the dev server's hosts and live pool state. Reads the
    /// running server's control socket when available, falling back to
    /// the configured hosts otherwise. Replaces the old `list`, which
    /// remains as a hidden alias.
    #[command(alias = "list")]
    Status {
        /// `server.toml` to inspect. Defaults to the bougied-managed
        /// config.
        #[arg(long, value_name = "PATH")]
        config: Option<std::path::PathBuf>,
    },
    /// Open the current project's (or NAME's) dev URL in a browser.
    Open {
        /// Hostname label to open. Defaults to the current project.
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    /// Stop the shared dev server. Equivalent to `bougie down server`;
    /// stops hosting for every project, since the server is shared.
    Stop,
    /// Tail the dev server's request log. In a project, defaults to
    /// this project's host.
    Logs {
        /// Follow the log; runs until interrupted (Ctrl-C).
        #[arg(short = 'f', long)]
        follow: bool,
        /// Number of trailing lines to print before any follow.
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    /// Manage local TLS via mkcert.
    #[command(subcommand)]
    Tls(ServerTlsCommand),
    /// Manage `/etc/hosts` overrides.
    #[command(subcommand)]
    Hosts(ServerHostsCommand),
}

#[derive(Subcommand, Debug)]
pub enum ServerHostsCommand {
    /// Rewrite the bougie sentinel block in /etc/hosts to match
    /// server.toml. Requires root — runs via sudo.
    Apply {
        /// `server.toml` to read the host list from. Defaults to the
        /// bougied-managed config.
        #[arg(long, value_name = "PATH")]
        config: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServerTlsCommand {
    /// Fetch mkcert and install bougie's local CA.
    Install,
    /// Uninstall bougie's local CA.
    Uninstall,
}

#[derive(Subcommand, Debug)]
pub enum ExtCommand {
    /// Add an extension dependency. Each `<arg>` is either an
    /// extension name (e.g. `redis`, `xdebug@3.5.1`) — fetched from
    /// the index and recorded in composer.json — or a path to a local
    /// `.so` file (e.g. `/opt/tideways/tideways-php-8.5.so`), in which
    /// case bougie copies it into the store, auto-detects the
    /// extension name and Zend-ness from the binary, and writes a
    /// fragment to `.bougie/conf.d-local/` without touching
    /// composer.json. Mix and match in one invocation.
    Add {
        /// Extension names or `.so` paths (anything ending in `.so` is
        /// treated as a local file).
        args: Vec<String>,
        /// Skip the implicit `bougie sync` after the composer call.
        #[arg(long)]
        no_sync: bool,
        #[command(flatten)]
        php: PhpPrefArgs,
    },
    /// Remove an extension dependency.
    Remove {
        /// The extension(s) to remove.
        names: Vec<String>,
        /// Skip the implicit `bougie sync` after the composer call.
        #[arg(long)]
        no_sync: bool,
    },
    /// List available extensions.
    List {
        /// Only show installed extensions.
        #[arg(long)]
        only_installed: bool,
        /// Only show extensions advertised by the index.
        #[arg(long)]
        only_available: bool,
        /// List all extension versions, including older releases.
        #[arg(long)]
        all_versions: bool,
        /// List extensions for all platforms, not just the host's.
        #[arg(long)]
        all_platforms: bool,
        /// Show the URLs of available extension downloads.
        #[arg(long)]
        show_urls: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum PhpCommand {
    /// Install a new PHP version.
    Install {
        /// The PHP version(s) to install (e.g. `8.3`, `8.3.12`, `8.3+zts`).
        requests: Vec<String>,
        /// Build flavor to install [possible values: nts, nts-debug, zts, zts-debug].
        #[arg(long)]
        flavor: Option<String>,
        /// Skip the entire baseline extension set; install only the bare
        /// Debian-aligned interpreter (`REFACTOR_DEBIAN_ALIGNED.md`).
        #[arg(long, conflicts_with = "without")]
        bare: bool,
        /// Skip a specific baseline extension. Repeatable: `--without opcache
        /// --without readline`. The named extensions must already be in the
        /// baseline set; use `bougie ext remove` after install for anything else.
        #[arg(long, value_name = "EXT", action = clap::ArgAction::Append)]
        without: Vec<String>,
    },
    /// Remove a PHP version.
    Uninstall {
        /// The PHP version(s) to uninstall.
        #[arg(required = true)]
        requests: Vec<String>,
        /// Build flavor to uninstall [possible values: nts, nts-debug, zts, zts-debug].
        #[arg(long)]
        flavor: Option<String>,
    },
    /// List available PHP interpreters.
    List {
        /// A PHP request to filter by.
        request: Option<String>,
        /// Only show installed PHP versions.
        #[arg(long)]
        only_installed: bool,
        /// Only show PHP versions available for download.
        #[arg(long)]
        only_available: bool,
        /// List all PHP versions, including older patch versions.
        #[arg(long)]
        all_versions: bool,
        /// List PHP downloads for all platforms.
        #[arg(long)]
        all_platforms: bool,
        /// List PHP downloads for all architectures.
        #[arg(long)]
        all_arches: bool,
        /// Show the URLs of available PHP downloads.
        #[arg(long)]
        show_urls: bool,
    },
    /// Search for a PHP interpreter.
    Find {
        /// A PHP request to search for.
        request: Option<String>,
    },
    /// Pin the project's PHP version.
    Pin {
        /// The PHP version to pin.
        request: String,
        /// Write the pin to `bougie.toml` (creating it if needed).
        #[arg(long, conflicts_with = "composer")]
        toml: bool,
        /// Write the pin to `composer.json`'s `require.php`.
        #[arg(long, conflicts_with = "toml")]
        composer: bool,
    },
    /// Refresh installed interpreters to the latest published patch.
    Upgrade {
        /// The PHP minor version(s) to upgrade (e.g. `8.3`).
        minor: Option<String>,
    },
    /// Show the PHP interpreter installation directory.
    Dir,
}

#[derive(Subcommand, Debug)]
pub enum ComposerCommand {
    /// Install a project's `vendor/` from `composer.lock`. Reads
    /// `composer.json` + `composer.lock` in the working directory,
    /// content-hash-verifies the lock, parallel-downloads dists into
    /// `vendor/`, and emits `vendor/autoload.php`.
    Install {
        /// Run the install in this directory instead of CWD.
        /// Mirrors Composer's `--working-dir` / `-d`.
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Skip dev-only packages and dev autoload entries.
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Fail if composer.lock is out of sync with composer.json.
        /// Currently a no-op — the install already errors on
        /// content-hash mismatch by default. Accepted for parity
        /// with Composer's CI usage.
        #[arg(long = "frozen")]
        frozen: bool,
        /// Verify the lock is internally consistent (content-hash,
        /// requires, transitives) and exit. Doesn't touch `vendor/`
        /// or run the autoloader. CI-friendly read-only check.
        #[arg(long = "lock-verify")]
        lock_verify: bool,
        /// Ignore all platform requirements (php, ext-*, lib-*).
        /// Accepted for Composer parity; bougie does not enforce
        /// platform requirements yet.
        #[arg(long = "ignore-platform-reqs")]
        ignore_platform_reqs: bool,
        /// Ignore a specific platform requirement.
        #[arg(long = "ignore-platform-req", value_name = "REQ")]
        ignore_platform_req: Vec<String>,
        /// Run composer.json root scripts, overriding `[scripts] run`
        /// in bougie.toml. Off by default (opt-in).
        #[arg(long, conflicts_with = "no_scripts")]
        scripts: bool,
        /// Skip composer.json root scripts, overriding `[scripts] run
        /// = true` in bougie.toml (Composer-compatible `--no-scripts`).
        #[arg(long = "no-scripts")]
        no_scripts: bool,
    },
    /// Resolve the project's dependency graph, write a fresh
    /// `composer.lock`, and install the result into `vendor/` (matching
    /// Composer's `update`). With no package arguments this re-resolves
    /// from scratch; naming one or more packages does a partial update —
    /// only those re-resolve while every other locked package stays
    /// pinned. `--no-install` stops after writing the lock; `--dry-run`
    /// previews the solution without writing anything. Aliased to
    /// `upgrade` / `u`, like Composer.
    #[command(visible_alias = "upgrade", alias = "u")]
    Update {
        /// Packages to update (`vendor/name`). When given, only these
        /// packages re-resolve; every other package stays pinned to its
        /// `composer.lock` version (Composer's partial update). With no
        /// packages, the whole graph re-resolves from scratch.
        #[arg(value_name = "PACKAGES")]
        packages: Vec<String>,
        /// Write the lock but don't install into `vendor/` (Composer's
        /// `--no-install`).
        #[arg(long = "no-install")]
        no_install: bool,
        /// Also update the named packages' dependencies (Composer's
        /// `--with-dependencies` / `-w`).
        #[arg(short = 'w', long = "with-dependencies")]
        with_dependencies: bool,
        /// Also update all of the named packages' dependencies, including
        /// ones shared with other packages (Composer's
        /// `--with-all-dependencies` / `-W`).
        #[arg(short = 'W', long = "with-all-dependencies")]
        with_all_dependencies: bool,
        /// Run the update in this directory instead of CWD.
        /// Mirrors Composer's `--working-dir` / `-d`.
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Skip dev-only root requires when resolving.
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Resolve and print the solution without writing
        /// `composer.lock` or touching `vendor/`. Without this flag,
        /// `update` writes a fresh `composer.lock`.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Ignore all platform requirements (php, ext-*, lib-*).
        /// Accepted for Composer parity; bougie does not enforce
        /// platform requirements yet.
        #[arg(long = "ignore-platform-reqs")]
        ignore_platform_reqs: bool,
        /// Ignore a specific platform requirement.
        #[arg(long = "ignore-platform-req", value_name = "REQ")]
        ignore_platform_req: Vec<String>,
    },
    /// Validate composer.json structure and contents.
    Validate {
        /// Run in this directory instead of CWD.
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Return non-zero exit code for warnings too.
        #[arg(long)]
        strict: bool,
        /// Skip lock file freshness check.
        #[arg(long = "no-check-lock")]
        no_check_lock: bool,
        /// Skip publish-only checks (name casing, required fields).
        #[arg(long = "no-check-publish")]
        no_check_publish: bool,
        /// Skip unbound/exact version constraint warnings.
        #[arg(long = "no-check-all")]
        no_check_all: bool,
        /// Also validate installed dependencies' composer.json files.
        #[arg(long = "with-dependencies")]
        with_dependencies: bool,
        /// Force lock file checking even when `config.lock` is false.
        #[arg(long = "check-lock")]
        check_lock: bool,
    },
    /// Regenerate `vendor/composer/autoload_*.php` against the current
    /// `composer.lock`. Drop-in for `composer dump-autoload`; output
    /// is byte-equivalent to Composer 2.8.12 with the same flags. Aliased
    /// to `dump-autoload` for users coming from Composer muscle-memory.
    #[command(alias = "dump-autoload")]
    DumpAutoloader {
        /// Optimize the classmap (`--optimize` / `-o`).
        #[arg(short = 'o', long = "optimize", alias = "optimize-autoloader")]
        optimize: bool,
        /// Emit the classmap-authoritative static loader
        /// (`--classmap-authoritative` / `-a`). Implies `--optimize`.
        #[arg(short = 'a', long = "classmap-authoritative")]
        classmap_authoritative: bool,
        /// Skip dev autoload entries (`--no-dev`).
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Emit the `APCu` loader bootstrap (`--apcu-autoloader`).
        #[arg(long = "apcu-autoloader")]
        apcu_autoloader: bool,
        /// Explicit `APCu` prefix; implies `--apcu-autoloader`.
        #[arg(long = "apcu-autoloader-prefix", value_name = "PREFIX")]
        apcu_prefix: Option<String>,
        /// Override the `ComposerAutoloaderInit<X>` class suffix —
        /// otherwise the value from `composer.json`'s
        /// `config.autoloader-suffix`, or the `composer.lock`
        /// content-hash.
        #[arg(long = "autoloader-suffix", value_name = "SUFFIX")]
        autoloader_suffix: Option<String>,
        /// Run the dump in this directory instead of the current one.
        /// Mirrors Composer's `--working-dir` / `-d`.
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Add one or more packages to `composer.json` `require` (or
    /// `require-dev`), re-resolve `composer.lock`, and install them.
    /// Fully Composer-compatible: a bare `vendor/pkg` resolves the
    /// latest stable and writes a caret (`^X.Y`) constraint; supply an
    /// explicit constraint with `vendor/pkg:^1.0`, `vendor/pkg=^1.0`, or
    /// a trailing argument (`vendor/pkg ^1.0`) — Composer's separators
    /// are `:`, `=`, or a space (the `@` separator is *not* accepted, as
    /// in Composer). For bougie's `>=`-default + `@`-syntax house style,
    /// use the top-level `bougie add` instead.
    Require {
        /// Packages to require, as Composer name↔version pairs.
        #[arg(value_name = "PACKAGES", required = true)]
        packages: Vec<String>,
        /// Add to `require-dev` instead of `require`.
        #[arg(long = "dev")]
        dev: bool,
        /// Edit `composer.json` only — don't re-resolve `composer.lock`
        /// or touch `vendor/` (Composer's `--no-update`).
        #[arg(long = "no-update")]
        no_update: bool,
        /// Re-resolve and write `composer.lock` but don't install into
        /// `vendor/` (Composer's `--no-install`).
        #[arg(long = "no-install")]
        no_install: bool,
        /// Also update the new packages' dependencies (`-w`).
        #[arg(short = 'w', long = "with-dependencies")]
        with_dependencies: bool,
        /// Also update all dependencies, including shared ones (`-W`).
        #[arg(short = 'W', long = "with-all-dependencies")]
        with_all_dependencies: bool,
        /// Prefer the lowest matching versions when resolving.
        #[arg(long = "prefer-lowest")]
        prefer_lowest: bool,
        /// Ignore all platform requirements (php, ext-*, lib-*).
        #[arg(long = "ignore-platform-reqs")]
        ignore_platform_reqs: bool,
        /// Ignore a specific platform requirement.
        #[arg(long = "ignore-platform-req", value_name = "REQ")]
        ignore_platform_req: Vec<String>,
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Resolve and report what would change without writing
        /// `composer.json`, `composer.lock`, or `vendor/`.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Remove one or more packages from `composer.json`, re-resolve
    /// `composer.lock`, and uninstall them from `vendor/`. Drop-in for
    /// `composer remove`.
    Remove {
        /// Packages to remove (`vendor/name`).
        #[arg(value_name = "PACKAGES", required = true)]
        packages: Vec<String>,
        /// Remove from `require-dev` instead of `require`.
        #[arg(long = "dev")]
        dev: bool,
        /// Edit `composer.json` only — don't re-resolve or touch
        /// `vendor/` (Composer's `--no-update`).
        #[arg(long = "no-update")]
        no_update: bool,
        /// Re-resolve and write `composer.lock` but don't touch
        /// `vendor/` (Composer's `--no-install`).
        #[arg(long = "no-install")]
        no_install: bool,
        /// Skip dev-only packages when resolving.
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Ignore all platform requirements (php, ext-*, lib-*).
        #[arg(long = "ignore-platform-reqs")]
        ignore_platform_reqs: bool,
        /// Ignore a specific platform requirement.
        #[arg(long = "ignore-platform-req", value_name = "REQ")]
        ignore_platform_req: Vec<String>,
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Resolve and report what would change without writing
        /// `composer.json`, `composer.lock`, or `vendor/`.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// List installed packages, or show details for one. Reads the
    /// project's `composer.lock`. Drop-in for `composer show` (aliases
    /// `info`, `list`).
    #[command(alias = "info", alias = "list")]
    Show {
        /// A single `vendor/name` to show details for. With no argument,
        /// every installed package is listed.
        #[arg(value_name = "PACKAGE")]
        package: Option<String>,
        /// Render the dependency tree (`--tree` / `-t`).
        #[arg(short = 't', long = "tree")]
        tree: bool,
        /// Only the project's direct dependencies (`--direct` / `-D`).
        #[arg(short = 'D', long = "direct")]
        direct: bool,
        /// Only platform packages — php, ext-*, lib-* (`--platform` / `-p`).
        #[arg(short = 'p', long = "platform")]
        platform: bool,
        /// Show the root package's own info (`--self` / `-s`).
        #[arg(short = 's', long = "self")]
        self_: bool,
        /// Print package names only (`--name-only` / `-N`).
        #[arg(short = 'N', long = "name-only")]
        name_only: bool,
        /// Show each package's install path (`--path` / `-P`).
        #[arg(short = 'P', long = "path")]
        path: bool,
        /// Also fetch and show the latest available version
        /// (`--latest` / `-l`).
        #[arg(short = 'l', long = "latest")]
        latest: bool,
        /// Only packages with a newer version available
        /// (`--outdated` / `-o`). Implies `--latest`.
        #[arg(short = 'o', long = "outdated")]
        outdated: bool,
        /// Skip dev dependencies (`--no-dev`).
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Show which packages depend on a given package — i.e. why it's
    /// installed. Drop-in for `composer why` (alias `depends`).
    #[command(alias = "depends")]
    Why {
        /// The package to explain.
        #[arg(value_name = "PACKAGE", required = true)]
        package: String,
        /// Recurse through the dependency chain (`--recursive` / `-r`).
        #[arg(short = 'r', long = "recursive")]
        recursive: bool,
        /// Render the full dependency-of tree (`--tree` / `-t`).
        #[arg(short = 't', long = "tree")]
        tree: bool,
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Show what prevents a package (optionally at a version) from being
    /// installed — conflicting requirements. Drop-in for
    /// `composer why-not` (alias `prohibits`).
    #[command(name = "why-not", alias = "prohibits")]
    WhyNot {
        /// The package to test.
        #[arg(value_name = "PACKAGE", required = true)]
        package: String,
        /// The version (or constraint) to test against. Defaults to `*`.
        #[arg(value_name = "VERSION")]
        version: Option<String>,
        /// Recurse through the dependency chain (`--recursive` / `-r`).
        #[arg(short = 'r', long = "recursive")]
        recursive: bool,
        /// Render the full tree (`--tree` / `-t`).
        #[arg(short = 't', long = "tree")]
        tree: bool,
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// List installed packages that have a newer version available.
    /// Drop-in for `composer outdated` (a focused `show --latest
    /// --outdated`). Use the global `--format json` for JSON output.
    Outdated {
        /// Optional `vendor/name` filters; with none, all packages are
        /// considered.
        #[arg(value_name = "PACKAGES")]
        packages: Vec<String>,
        /// Only the project's direct dependencies (`--direct` / `-D`).
        #[arg(short = 'D', long = "direct")]
        direct: bool,
        /// Only show packages with a new major version (`--major-only`).
        #[arg(long = "major-only")]
        major_only: bool,
        /// Only show packages with a new minor version (`--minor-only`).
        #[arg(long = "minor-only")]
        minor_only: bool,
        /// Only show packages with a new patch version (`--patch-only`).
        #[arg(long = "patch-only")]
        patch_only: bool,
        /// Skip dev dependencies (`--no-dev`).
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Exit non-zero if any package is outdated (`--strict`).
        #[arg(long = "strict")]
        strict: bool,
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Check installed packages against the Packagist security-advisories
    /// database. Drop-in for `composer audit`. Exits non-zero when
    /// advisories are found. Use the global `--format json` for JSON.
    Audit {
        /// Skip dev dependencies (`--no-dev`).
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// How to treat abandoned packages (`--abandoned`). Currently
        /// accepted for parity; abandoned detection is not yet wired.
        #[arg(long = "abandoned", value_enum, default_value = "report")]
        abandoned: AbandonedHandling,
        /// Audit the locked set (`--locked`). bougie always reads
        /// `composer.lock`, so this is the default behavior; accepted
        /// for parity.
        #[arg(long = "locked")]
        locked: bool,
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// List the license of every installed package. Drop-in for
    /// `composer licenses`. Use the global `--format json` for JSON.
    Licenses {
        /// Skip dev dependencies (`--no-dev`).
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Report packages that look locally modified. Drop-in for
    /// `composer status`. bougie installs from dist archives, so for the
    /// common case this reports "no local changes".
    Status {
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Show funding information for installed packages, grouped by
    /// vendor. Drop-in for `composer fund`. Use `--format json` for JSON.
    Fund {
        /// Skip dev dependencies (`--no-dev`).
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Run in this directory instead of CWD (`-d`).
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Catch-all for any composer subcommand bougie does not implement
    /// natively (`create-project`, `archive`, `bump`, `global`, …).
    /// bougie does not bundle the Composer phar, so these no longer run;
    /// the dispatch returns an error pointing at
    /// `bougie tool install composer/composer` for the full upstream
    /// Composer.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

/// How `composer audit` treats abandoned packages.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbandonedHandling {
    /// Ignore abandoned packages entirely.
    Ignore,
    /// Report abandoned packages but don't fail on them.
    Report,
    /// Treat abandoned packages as an audit failure.
    Fail,
}

#[derive(Subcommand, Debug)]
pub enum CacheCommand {
    /// Wipe the full cache.
    Clean,
    /// Remove unneeded library files.
    Prune {
        /// Show what would be pruned without removing anything.
        #[arg(long)]
        dry_run: bool,
        /// Also remove tracked projects that no longer exist on disk.
        #[arg(long)]
        prune_projects: bool,
    },
    /// Show the location of the cache directory.
    Dir,
    /// Show the cache size.
    Size,
}

#[derive(Subcommand, Debug)]
pub enum ToolCommand {
    /// Install a tool. Pass `<vendor>/<name>` optionally followed by
    /// `@<constraint>` (e.g. `phpstan/phpstan@^1.10`).
    Install {
        /// Composer package identifier, optionally with `@<constraint>`.
        package: String,
        /// Pin the tool to a specific PHP. Accepts a version (`8.3`,
        /// `8.3.12`) or a constraint (`~8.3`, `>=8.2,<8.4`). When the
        /// requested PHP isn't installed, bougie installs it
        /// automatically. Defaults to the highest installed NTS PHP.
        #[arg(long, value_name = "VER")]
        php: Option<String>,
        /// Additional Composer package (`vendor/name[@<constraint>]`)
        /// or PHP extension (`intl`, `redis`) to install alongside the
        /// tool. May be passed multiple times.
        #[arg(long, value_name = "PKG_OR_EXT")]
        with: Vec<String>,
        /// Overwrite an existing executable at the bin-dir path.
        #[arg(long)]
        force: bool,
    },
    /// Remove an installed tool by its `<vendor>/<name>` identifier.
    Uninstall {
        /// Composer package identifier.
        package: String,
    },
    /// Add an extra composer package or PHP extension to an
    /// installed tool. Re-resolves the tool's lock and updates the
    /// vendor tree in place.
    Inject {
        /// Composer package identifier of the tool.
        package: String,
        /// Extra to add (`vendor/name[@<constraint>]` for composer
        /// packages, bare name for PHP extensions). Repeatable.
        #[arg(long, value_name = "PKG_OR_EXT", required = true)]
        with: Vec<String>,
    },
    /// Remove an extra previously added via `--with` / `inject`.
    Uninject {
        /// Composer package identifier of the tool.
        package: String,
        /// Extra to remove. Repeatable.
        #[arg(long, value_name = "PKG_OR_EXT", required = true)]
        with: Vec<String>,
    },
    /// List installed tools.
    List,
    /// Print a tool's install directory, or the tools root if no
    /// package is given.
    Dir {
        /// Composer package identifier; omit to print the tools root.
        package: Option<String>,
    },
    /// Run an installed-or-cached tool one-off. Reuses an existing
    /// persistent install if `(package, constraint, php, with)` match
    /// exactly; otherwise materialises into the ephemeral cache.
    ///
    /// `bgx` is provided as a convenient alias for `bougie tool run`;
    /// their behavior is identical.
    #[command(
        after_help = "Use `bgx` as a shortcut for `bougie tool run`.\n\n\
                      Use `bougie help tool run` for more details.",
        after_long_help = ""
    )]
    Run(ToolRunArgs),
    // Hidden alias for `bougie tool run` for the `bgx` command. The
    // variant is reached only via the `bgx` binary exec'ing into it;
    // it doesn't surface under `bougie tool --help`. Carrying it as
    // a separate variant (with `display_name`, `override_usage`)
    // lets clap render `bgx --help` and clap-level error messages
    // with `bgx` as the program name rather than leaking
    // `bougie tool run`.
    #[command(
        hide = true,
        override_usage = "bgx [OPTIONS] <PACKAGE> [ARGS]...",
        about = "Run a tool from a Composer package.",
        long_about = None,
        after_help = "Use `bougie help tool run` for more details.",
        after_long_help = "",
        display_name = "bgx",
        // `bgx --version` / `bgx -V` exec into `bougie tool bgx`; give
        // this variant its own version flag so it short-circuits before
        // the required `<PACKAGE>` positional and prints `bgx <version>`.
        version = LONG_VERSION
    )]
    Bgx(BgxArgs),
    /// Re-resolve a tool's lock and bring its vendor tree up to date.
    /// Pass `--all` to walk every installed tool, or `--reinstall` to
    /// wipe and rebuild from scratch (recovery for broken state).
    Upgrade {
        /// Composer package identifier. Required unless `--all`.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        package: Option<String>,
        /// Upgrade every installed tool.
        #[arg(long)]
        all: bool,
        /// Wipe the tool dir + every entrypoint symlink and reinstall
        /// from scratch using the receipt's pinned `(package,
        /// constraint, php_version, with, extensions)` tuple.
        #[arg(long)]
        reinstall: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SelfCommand {
    /// Update bougie.
    Update {
        /// Update even when bougie can't confirm it installed this
        /// binary. By default `self update` only touches a binary that
        /// bougie's own installer placed (per the install receipt);
        /// copies from a package manager, cargo, or nix are left for
        /// that tool to update. Pass `--force` only if you know this
        /// copy came from bougie's installer.
        #[arg(long)]
        force: bool,
    },
    /// Show bougie's version.
    Version {
        /// Only show the version.
        #[arg(long)]
        short: bool,
    },
}

#[derive(Args, Debug)]
pub struct ToolRunArgs {
    /// Composer package identifier, optionally with `@<constraint>`.
    pub package: String,
    /// Pin the tool to a specific PHP for this run.
    #[arg(long, value_name = "VER")]
    pub php: Option<String>,
    /// Extra composer package or PHP extension, same shape as
    /// `tool install --with`. Repeatable.
    #[arg(long, value_name = "PKG_OR_EXT")]
    pub with: Vec<String>,
    /// Arguments forwarded to the tool. Use `--` to separate when
    /// forwarding flags that bougie would otherwise parse.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<std::ffi::OsString>,
}

/// Args for the hidden `bgx` alias. Wraps [`ToolRunArgs`] verbatim so
/// the two variants share their entire surface; the wrapper exists
/// only so clap renders help / errors with `bgx` as the program name.
#[derive(Args, Debug)]
pub struct BgxArgs {
    #[command(flatten)]
    pub tool_run: ToolRunArgs,
}
