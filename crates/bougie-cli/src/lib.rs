use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, Parser, Subcommand};
use std::ffi::OsString;

/// Full version string, uv-style: `0.6.4 (63c5f57d3 2026-05-08 <target>)`.
///
/// Built by `build.rs`; degrades to the bare crate version when git metadata is
/// unavailable. clap prefixes the binary name, so `--version` prints
/// `bougie 0.6.4 (...)`.
pub const LONG_VERSION: &str = env!("BOUGIE_LONG_VERSION");

/// Short (9-char) git SHA of this build, when git metadata was
/// available at build time. `None` for crates.io tarball builds —
/// telemetry treats absence as the `cargo` install channel.
pub const BUILD_SHA: Option<&str> = option_env!("BOUGIE_BUILD_SHA");

const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Blue.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Magenta.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::BrightMagenta.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::BrightMagenta.on_default())
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
    .valid(AnsiColor::Green.on_default())
    .invalid(AnsiColor::Yellow.on_default().effects(Effects::BOLD));

/// Grouped quick-reference appended to `bougie --help`. clap renders all
/// subcommands in one flat "Commands:" list (it has no headed-group
/// support), and `display_order` clusters that list into these same
/// groups — this cheat-sheet just names the groups so the core verbs are
/// findable at a glance.
const COMMAND_GROUPS: &str = "\
Command groups:
  Project      init, new, start, stop, run, sync, make, format
  Dependencies add, remove, lock, tree, outdated, ext, composer
  Toolchain    php, node, tool
  Services     server, service, projects
  Admin        cache, self

Run `bougie help <command>` for details on any command";

#[derive(Parser, Debug)]
#[command(
    name = "bougie",
    version = LONG_VERSION,
    about = "PHP toolchain management, the luxury way",
    long_about = "PHP toolchain management, the luxury way\n\nManage your PHP installations, background services, extensions and dependencies quickly with bougie",
    styles = HELP_STYLES,
    after_long_help = COMMAND_GROUPS,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Suppress non-error output
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Output format
    #[arg(long, global = true, default_value = "text")]
    pub format: OutputFormat,
}

/// Shared PHP-source preference flags (uv's system-Python model adapted
/// to PHP). Flattened into `sync` / `run`; `--managed-php` and
/// `--no-managed-php` are mutually exclusive. With none set, bougie's
/// default applies: prefer an installed managed PHP, then download one.
/// Only a one-off `bougie run` also reaches for a qualifying system PHP
/// (before downloading) — used for that invocation only, never pinned;
/// configuring a *project* against a system PHP always requires the
/// explicit `--no-managed-php` / `[php] managed = false` opt-in.
#[derive(Args, Debug, Clone, Copy, Default)]
pub struct PhpPrefArgs {
    /// Only use a bougie-managed PHP; never a system PHP
    #[arg(long, conflicts_with = "no_managed_php")]
    pub managed_php: bool,
    /// Only use a system PHP already on this machine; never a managed one
    #[arg(long)]
    pub no_managed_php: bool,
    /// Never download a managed PHP — use an installed one (or, for a
    /// one-off `bougie run`, a system PHP). Errors otherwise
    #[arg(long)]
    pub no_php_downloads: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    /// `json-v1` is bougie's structured envelope; `json` is accepted as
    /// an alias so the `composer` subcommands (`composer show --format
    /// json`, etc.) work with the same global flag
    #[value(name = "json-v1", alias = "json")]
    JsonV1,
}

/// Version-preference policy for a resolve, mirroring uv's
/// `--resolution`. Maps onto `bougie-composer-resolver`'s
/// `ResolutionStrategy` in the dispatch layer.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResolutionStrategy {
    /// Prefer the newest compatible version of every package (the default)
    #[default]
    Highest,
    /// Prefer the oldest compatible version of every package, including
    /// transitive dependencies
    Lowest,
    /// Prefer the oldest compatible version of the project's direct
    /// requires, but the newest for everything they pull in transitively
    #[value(name = "lowest-direct")]
    LowestDirect,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a new project
    #[command(display_order = 1)]
    Init {
        /// Scaffold a single self-contained script at FILE instead of a
        /// project (uv's `uv init --script`): a `bougie run --script`
        /// shebang + a `# /// script` composer.json block. Other flags
        /// are ignored in this mode
        #[arg(long = "script", value_name = "FILE")]
        script: Option<std::path::PathBuf>,
        /// Place bougie configuration in a bougie.toml file
        #[arg(long)]
        toml: bool,
        /// Set the package name (`vendor/package`) of the generated
        /// composer.json. Overrides the name from a `--starter` manifest
        #[arg(long, value_name = "VENDOR/PACKAGE")]
        name: Option<String>,
        /// Scaffold from a starter pack: a built-in alias (e.g. `mageos`)
        /// or an https URL serving a starter manifest. Writes the
        /// starter's composer.json instead of the empty default
        #[arg(long, value_name = "URL_OR_ALIAS")]
        starter: Option<String>,
        /// After scaffolding, bring the project up. Equivalent to
        /// `bougie start`
        #[arg(long)]
        start: bool,
    },

    /// Create a new project in a new directory
    #[command(display_order = 2)]
    New {
        /// Directory to create under the current directory and scaffold
        /// the project into
        #[arg(value_name = "DIRECTORY")]
        directory: String,
        /// Place bougie configuration in a bougie.toml file
        #[arg(long)]
        toml: bool,
        /// Set the package name (`vendor/package`) of the generated
        /// composer.json. Overrides the name from a `--starter` manifest
        #[arg(long, value_name = "VENDOR/PACKAGE")]
        name: Option<String>,
        /// Scaffold from a starter pack: a built-in alias (e.g. `mageos`)
        /// or an https URL serving a starter manifest
        #[arg(long, value_name = "URL_OR_ALIAS")]
        starter: Option<String>,
        /// After scaffolding, bring the project up. Equivalent to
        /// `bougie start`
        #[arg(long)]
        start: bool,
    },

    /// Manage PHP extensions
    #[command(subcommand, display_order = 15)]
    Ext(ExtCommand),

    /// Manage patches applied to installed packages
    #[command(subcommand, display_order = 16)]
    Patches(PatchesCommand),

    /// Add dependencies to the project
    #[command(display_order = 10)]
    Add {
        /// Packages to add, `vendor/pkg` or `vendor/pkg@<constraint>`
        #[arg(value_name = "PACKAGES", required = true)]
        packages: Vec<String>,
        /// Add the packages to a self-contained script's inline
        /// `# /// script` block instead of a project's composer.json
        /// (uv's `uv add --script`), then refresh its adjacent
        /// `<file>.lock`
        #[arg(long = "script", value_name = "FILE")]
        script: Option<std::path::PathBuf>,
        /// Add to `require-dev` instead of `require`
        #[arg(long = "dev")]
        dev: bool,
        /// Also update the new packages' dependencies (`-w`)
        #[arg(short = 'w', long = "with-dependencies")]
        with_dependencies: bool,
        /// Also update all dependencies, including shared ones (`-W`)
        #[arg(short = 'W', long = "with-all-dependencies")]
        with_all_dependencies: bool,
        /// Update `composer.json` + `composer.lock` but don't install
        /// into `vendor/`
        #[arg(long = "no-sync")]
        no_sync: bool,
        /// Edit `composer.json` only — don't touch the lock or `vendor/`
        #[arg(long = "frozen")]
        frozen: bool,
        /// Version-preference policy when resolving
        #[arg(long = "resolution", value_name = "STRATEGY", default_value = "highest")]
        resolution: ResolutionStrategy,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Resolve and report what would change without writing anything
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Ignore all platform requirements (php, ext-*, lib-*) when resolving
        #[arg(long = "ignore-platform-reqs")]
        ignore_platform_reqs: bool,
        /// Ignore a specific platform requirement (`php`, `ext-gd`, …);
        /// repeatable, `*` wildcards allowed
        #[arg(long = "ignore-platform-req", value_name = "REQ")]
        ignore_platform_req: Vec<String>,
    },

    /// Remove dependencies from the project
    #[command(display_order = 11)]
    Remove {
        /// Packages to remove (`vendor/name`)
        #[arg(value_name = "PACKAGES", required = true)]
        packages: Vec<String>,
        /// Remove from `require-dev` instead of `require`
        #[arg(long = "dev")]
        dev: bool,
        /// Re-resolve `composer.lock` but don't touch `vendor/`
        #[arg(long = "no-sync")]
        no_sync: bool,
        /// Edit `composer.json` only — don't touch the lock or `vendor/`
        #[arg(long = "frozen")]
        frozen: bool,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Resolve and report what would change without writing anything
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Ignore all platform requirements (php, ext-*, lib-*) when resolving
        #[arg(long = "ignore-platform-reqs")]
        ignore_platform_reqs: bool,
        /// Ignore a specific platform requirement (`php`, `ext-gd`, …);
        /// repeatable, `*` wildcards allowed
        #[arg(long = "ignore-platform-req", value_name = "REQ")]
        ignore_platform_req: Vec<String>,
    },

    /// Update the project's lockfile
    #[command(display_order = 12)]
    Lock {
        /// Lock a single self-contained script's inline dependencies into
        /// an adjacent `<file>.lock` (uv's `uv lock --script`). `bougie
        /// run --script` then installs from that lock for reproducible,
        /// offline runs instead of re-resolving
        #[arg(long = "script", value_name = "FILE")]
        script: Option<std::path::PathBuf>,
        /// Version-preference policy when re-resolving changed requires
        #[arg(long = "resolution", value_name = "STRATEGY", default_value = "highest")]
        resolution: ResolutionStrategy,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Resolve and report what would change without writing the lock
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Ignore all platform requirements (php, ext-*, lib-*) when resolving
        #[arg(long = "ignore-platform-reqs")]
        ignore_platform_reqs: bool,
        /// Ignore a specific platform requirement (`php`, `ext-gd`, …);
        /// repeatable, `*` wildcards allowed
        #[arg(long = "ignore-platform-req", value_name = "REQ")]
        ignore_platform_req: Vec<String>,
    },

    /// Display the project's dependency tree
    #[command(display_order = 13)]
    Tree {
        /// Root the tree at this package instead of the project
        #[arg(value_name = "PACKAGE")]
        package: Option<String>,
        /// Skip dev dependencies
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },

    /// List installed packages with newer versions available
    #[command(display_order = 14)]
    Outdated {
        /// Optional `vendor/name` filters; with none, all are considered
        #[arg(value_name = "PACKAGES")]
        packages: Vec<String>,
        /// Only the project's direct dependencies (`--direct` / `-D`)
        #[arg(short = 'D', long = "direct")]
        direct: bool,
        /// Only packages with a new major version
        #[arg(long = "major-only")]
        major_only: bool,
        /// Only packages with a new minor version
        #[arg(long = "minor-only")]
        minor_only: bool,
        /// Only packages with a new patch version
        #[arg(long = "patch-only")]
        patch_only: bool,
        /// Skip dev dependencies
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Exit non-zero if any package is outdated
        #[arg(long = "strict")]
        strict: bool,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },

    /// Install everything the project requires
    #[command(display_order = 6)]
    Sync {
        /// Don't try to download anything, this will fail if there are uncached packages
        #[arg(long)]
        offline: bool,
        /// Show the plan, change nothing on disk
        #[arg(long)]
        dry_run: bool,
        /// Run composer.json root scripts for this sync, overriding
        /// `[scripts] run` in bougie.toml. Off by default (opt-in)
        #[arg(long, conflicts_with = "no_scripts")]
        scripts: bool,
        /// Skip composer.json root scripts for this sync, overriding
        /// `[scripts] run = true` in bougie.toml
        #[arg(long = "no-scripts")]
        no_scripts: bool,
        /// Version-preference policy when a fresh lock must be resolved.
        /// No effect when a `composer.lock` already exists
        #[arg(long = "resolution", value_name = "STRATEGY", default_value = "highest")]
        resolution: ResolutionStrategy,
        /// Apply patches for this sync, overriding `[patches] enable`.
        /// On by default when patches are declared
        #[arg(long, conflicts_with = "no_patches")]
        patches: bool,
        /// Skip native patch application for this sync
        #[arg(long = "no-patches")]
        no_patches: bool,
        /// Ignore all platform requirements (php, ext-*, lib-*) when a fresh
        /// lock must be resolved
        #[arg(long = "ignore-platform-reqs")]
        ignore_platform_reqs: bool,
        /// Ignore a specific platform requirement (`php`, `ext-gd`, …);
        /// repeatable, `*` wildcards allowed
        #[arg(long = "ignore-platform-req", value_name = "REQ")]
        ignore_platform_req: Vec<String>,
        #[command(flatten)]
        php: PhpPrefArgs,
    },

    /// Run a command or script
    #[command(display_order = 5)]
    Run {
        /// Treat the first argument as a self-contained PHP script with an
        /// inline `# /// script` composer.json block (uv's `--script`):
        /// resolve its declared `php` / `ext-*` / package requires into an
        /// ephemeral cached environment and run it with the autoloader
        /// prepended. Skips project sync and the composer-script lookup.
        /// This is what the `#!/usr/bin/env -S bougie run --script`
        /// shebang invokes
        #[arg(long)]
        script: bool,
        /// Add a temporary extension for this invocation
        #[arg(long, value_name = "EXT=VER")]
        with: Vec<String>,
        /// Skip the implicit `bougie sync` before running
        #[arg(long)]
        no_sync: bool,
        /// Layer the server's debug overlay (`vendor/bougie/conf.d-debug/`)
        /// into `PHP_INI_SCAN_DIR` and set `XDEBUG_SESSION=1` for the
        /// child. Installs xdebug on first use if not already present
        #[arg(long)]
        xdebug: bool,
        /// Run with a specific PHP interpreter. Accepts a version
        /// (`8.3`, `8.3.12`), a constraint (`~8.3`, `>=8.2,<8.4`), or a
        /// path to a `php` binary. Forces a sync to that interpreter,
        /// so it can't be combined with `--no-sync`
        #[arg(long = "php", value_name = "VER|PATH", conflicts_with = "no_sync")]
        php_request: Option<String>,
        #[command(flatten)]
        php: PhpPrefArgs,
        /// Command and arguments. `--` separator is optional
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        argv: Vec<String>,
    },

    /// Manage PHP interpreters
    #[command(subcommand, display_order = 20)]
    Php(PhpCommand),

    /// Manage Node.js interpreters
    #[command(subcommand, display_order = 21)]
    Node(NodeCommand),

    /// Manage PHP packages with a composer compatible interface
    #[command(subcommand, display_order = 16)]
    Composer(ComposerCommand),

    /// Run and install commands provided by PHP packages
    #[command(subcommand, display_order = 22)]
    Tool(ToolCommand),

    /// Runtime shim invoked by tool wrappers (`#!.../bougie tool-exec`).
    /// Not for direct CLI use; hidden from `--help`
    #[command(hide = true, name = "tool-exec")]
    ToolExec {
        /// Path to the tool wrapper script the kernel handed us as
        /// argv[1] via the shebang
        wrapper: std::path::PathBuf,
        /// User-supplied arguments to the tool, passed through to PHP
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },

    /// Manage bougie's cache
    #[command(subcommand, display_order = 40)]
    Cache(CacheCommand),

    /// Manage the bougie binary itself
    #[command(subcommand)]
    #[command(name = "self", display_order = 41)]
    SelfCmd(SelfCommand),

    /// Manage anonymous usage telemetry (opt-in; see TELEMETRY.md)
    #[command(display_order = 42)]
    Telemetry {
        #[command(subcommand)]
        command: Option<TelemetryCommand>,
    },

    /// Internal: upload spooled telemetry. Spawned detached by bougie
    /// itself; hidden from help and not for direct use
    #[command(hide = true, name = "__telemetry-flush")]
    TelemetryFlush,

    /// Assemble a shareable diagnostic report from the last failure
    /// (shown in full for review; nothing is sent without confirmation)
    #[command(display_order = 43)]
    Diagnose {
        /// Render a prefilled GitHub issue instead of uploading
        #[arg(long)]
        issue: bool,
        /// Upload without the interactive confirmation
        #[arg(short = 'y', long)]
        yes: bool,
        /// Re-run a bougie command with debug logging captured:
        /// `bougie diagnose -- sync --offline`
        #[arg(allow_hyphen_values = true, last = true)]
        args: Vec<OsString>,
    },

    /// Run the bougie development HTTP server
    #[command(display_order = 30)]
    Server(ServerArgs),

    /// Manage project-scoped dev services (formerly `services`, which
    /// remains as a hidden alias)
    #[command(subcommand, display_order = 31, alias = "services")]
    Service(ServiceCommand),

    /// Inspect and manage provisioned tenants
    #[command(subcommand, display_order = 32)]
    Projects(ProjectsCommand),

    /// Bring the whole project up
    #[command(display_order = 3)]
    Start {
        /// Skip the implicit `bougie sync` prologue
        #[arg(long)]
        no_sync: bool,
        /// Show what would run, but don't execute
        #[arg(long)]
        dry_run: bool,
        /// Explain why each step runs or skips
        #[arg(long)]
        explain: bool,
        /// Ignore the builtin recipe; use only `bougie.toml`
        #[arg(long)]
        no_builtin: bool,
        /// Force a specific builtin (e.g. `magento`)
        #[arg(long, value_name = "NAME")]
        recipe: Option<String>,
    },

    /// Bring the project down
    #[command(display_order = 4)]
    Stop {
        /// Service names to stop. Empty = every declared service
        names: Vec<String>,
        /// Destroy persisted tenant data (e.g. FLUSHDB on redis). Off
        /// by default — `bougie start` should restore state
        #[arg(long)]
        purge: bool,
    },

    /// Run project tasks
    #[command(display_order = 7)]
    Make {
        /// Task to run. With none, the available tasks are listed
        task: Option<String>,
        /// List available tasks instead of running
        #[arg(long, conflicts_with_all = ["dry_run", "explain", "print"])]
        list: bool,
        /// Show what would run, but don't execute
        #[arg(long)]
        dry_run: bool,
        /// Explain why each step runs or skips
        #[arg(long)]
        explain: bool,
        /// Skip the implicit `bougie sync` prologue
        #[arg(long)]
        no_sync: bool,
        /// Ignore the builtin recipe; use only `bougie.toml`
        #[arg(long)]
        no_builtin: bool,
        /// Force a specific builtin (e.g. `magento`)
        #[arg(long, value_name = "NAME")]
        recipe: Option<String>,
        /// Print the merged recipe to stdout instead of running
        #[arg(long)]
        print: bool,
    },

    /// Format the project's PHP code
    #[command(display_order = 8)]
    Format {
        /// Arguments forwarded verbatim to `wick` (paths, `--check`,
        /// `--diff`, `-` for stdin, …)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "ARGS")]
        args: Vec<std::ffi::OsString>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServiceCommand {
    /// Start the project's declared services (or every service in
    /// `names`) and provision the project's tenant in each. For the
    /// whole-project bring-up use `bougie start`
    Up {
        /// Service names to bring up. Empty = every declared service
        names: Vec<String>,
        /// Start the services and return immediately instead of
        /// attaching to their combined log stream. Attaching is the
        /// default for an interactive (TTY) text-mode invocation;
        /// non-interactive runs and `--format json-v1` always detach
        #[arg(short = 'd', long)]
        detach: bool,
    },
    /// Stop the project's declared services (or every service in
    /// `names`). The shared global process stays up while any other
    /// project's tenant remains. For the whole-project teardown use
    /// `bougie stop`
    Down {
        names: Vec<String>,
        /// Destroy persisted tenant data (e.g. FLUSHDB on redis). Off
        /// by default — re-adding the service should restore state
        #[arg(long)]
        purge: bool,
    },
    /// Declare a service in the project. Errors if the name isn't in
    /// the catalog. Use `bougie service catalog` to discover names
    Add {
        /// One or more service names, each optionally `@<version>`
        names: Vec<String>,
    },
    /// Remove a service declaration from the project. Tenant data is
    /// kept by default (re-adding restores it); pass `--purge` to also
    /// destroy it
    Remove {
        /// Service names to remove
        names: Vec<String>,
        /// Also destroy the project's tenant data for each service
        /// (same as `bougie service down --purge`) before undeclaring
        #[arg(long)]
        purge: bool,
    },
    /// List the services declared in the current project
    List {
        /// Reserved for cross-project listing in Phase 3+. Today this
        /// degrades silently to per-project output
        #[arg(long)]
        all: bool,
    },
    /// Print the built-in service catalog (no daemon required)
    Catalog,
    /// Run a service client tool (mariadb, mysqldump, redis-cli,
    /// rabbitmqctl, …) wired to this project's tenant. Curated tools
    /// are also linked into `vendor/bougie/bin/`, so inside `bougie run`
    /// they resolve by bare name; this verb additionally reaches any
    /// uncurated binary in a declared service's bin/ or sbin/
    Exec {
        /// Restrict resolution to one service (needed when the tool
        /// name exists in several, or isn't in the curated list)
        #[arg(long)]
        service: Option<String>,
        /// Tool name (e.g. mysqldump, redis-cli, opensearch-plugin)
        tool: String,
        /// Arguments forwarded to the tool verbatim
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "ARGS")]
        args: Vec<std::ffi::OsString>,
    },
    /// Restart the named services (or every declared service). Stops
    /// then starts the underlying global process; the tenant ledger
    /// is preserved, so generated passwords / DB numbers survive.
    /// Affects every project sharing the same service
    Restart {
        names: Vec<String>,
    },
    /// Per-service status for the current project
    Status {
        /// Limit to a single service
        name: Option<String>,
    },
    /// Print this project's tenant connection info — including
    /// passwords — for wiring up external clients (GUI database
    /// tools, API explorers, …). Reads the on-disk tenant ledgers;
    /// no daemon required. Inside `bougie run` the same values are
    /// already injected as `BOUGIE_SERVICE_*` env vars
    Credentials {
        /// Limit to a single service (works even after `bougie
        /// service remove`, as long as the tenant is provisioned)
        name: Option<String>,
        /// Emit `KEY='value'` lines using the exact `BOUGIE_SERVICE_*`
        /// names `bougie run` injects, for `eval` in a plain shell.
        /// Takes precedence over `--format`
        #[arg(long)]
        env: bool,
    },
    /// Tail (and optionally follow) service logs. With no name, shows
    /// the combined ("multilog") stream of every service declared in the
    /// project, each line prefixed with its (colorized) service name —
    /// the same view `bougie service up` attaches to
    Logs {
        /// Service name. Omit to tail every declared service at once
        name: Option<String>,
        /// Follow the log; runs until interrupted (Ctrl-C)
        #[arg(short = 'f', long)]
        follow: bool,
        /// Number of trailing lines to print before any follow
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    /// Inspect and control the `bougied` daemon
    #[command(subcommand)]
    Daemon(ServiceDaemonCommand),
}

#[derive(Subcommand, Debug)]
pub enum ProjectsCommand {
    /// List every provisioned tenant across the shared services and the
    /// project each belongs to. Reads the on-disk tenant ledgers; no
    /// daemon required
    List {
        /// Show the per-service allocation (redis db number, rabbitmq
        /// vhost, server hostname, …) as an extra column
        #[arg(long)]
        alloc: bool,
    },
    /// Deprovision tenants and remove them from the service ledgers.
    /// With no flags, targets *orphaned* tenants whose project directory
    /// no longer exists. Destructive: when the service is running this
    /// drops the tenant's data (database, vhost, redis db, …); when it's
    /// stopped, only the ledger entry is removed
    Purge {
        /// Purge a specific project's tenants by path (it may already be
        /// deleted) instead of the orphaned set
        #[arg(long)]
        project: Option<String>,
        /// Purge every tenant of every project. Use with care
        #[arg(long)]
        all: bool,
        /// Print what would be purged and exit without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt (required for non-interactive use)
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServiceDaemonCommand {
    /// Print daemon PID, socket path, and managed-service count. The
    /// daemon is auto-spawned if not already running
    Status,
    /// Send a graceful shutdown to the running daemon
    Stop,
    /// Print the daemon's reported version (used by the CLI to detect
    /// post-`self update` daemon-binary mismatches)
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
    /// Defaults to a name derived from the project
    #[arg(value_name = "NAME")]
    pub name: Option<String>,
    /// Open the project URL in a browser once the server is ready
    #[arg(long)]
    pub open: bool,
    /// Serve over HTTPS (requires `bougie server tls install`)
    #[arg(long)]
    pub tls: bool,
    /// Print the URL and return immediately instead of attaching to the
    /// log stream. Matches `service up`'s `-d`
    #[arg(short = 'd', long = "detach")]
    pub detach: bool,
    /// Skip the implicit `bougie sync` before serving
    #[arg(long)]
    pub no_sync: bool,
}

#[derive(Subcommand, Debug)]
pub enum ServerCommand {
    /// Low-level primitive: run the server process against an explicit
    /// multi-host `server.toml`, foreground, with no daemon. This is
    /// what `bougied` spawns and what CI / power users invoke directly;
    /// `--config` is required because a multi-host server has no single
    /// project to default to. The bougied-managed path (`bougie service
    /// up server`) supplies its own service-scoped `server.toml`
    Run {
        /// `server.toml` path. Required
        #[arg(long, value_name = "PATH")]
        config: std::path::PathBuf,
        /// CLI override of `[server].listen` (e.g. `127.0.0.1:7080`)
        #[arg(long, value_name = "ADDR")]
        listen: Option<String>,
        /// CLI override of `[server].log_format`
        #[arg(long, value_name = "FMT")]
        log_format: Option<String>,
    },
    /// Show the dev server's hosts and live pool state. Reads the
    /// running server's control socket when available, falling back to
    /// the configured hosts otherwise. Replaces the old `list`, which
    /// remains as a hidden alias
    #[command(alias = "list")]
    Status {
        /// `server.toml` to inspect. Defaults to the bougied-managed
        /// config
        #[arg(long, value_name = "PATH")]
        config: Option<std::path::PathBuf>,
    },
    /// Open the current project's (or NAME's) dev URL in a browser
    Open {
        /// Hostname label to open. Defaults to the current project
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    /// Stop the shared dev server. Equivalent to `bougie service down
    /// server`; stops hosting for every project, since the server is shared
    Stop,
    /// Tail the dev server's request log. In a project, defaults to
    /// this project's host
    Logs {
        /// Follow the log; runs until interrupted (Ctrl-C)
        #[arg(short = 'f', long)]
        follow: bool,
        /// Number of trailing lines to print before any follow
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    /// Manage local TLS via mkcert
    #[command(subcommand)]
    Tls(ServerTlsCommand),
    /// Manage `/etc/hosts` overrides
    #[command(subcommand)]
    Hosts(ServerHostsCommand),
}

#[derive(Subcommand, Debug)]
pub enum ServerHostsCommand {
    /// Rewrite the bougie sentinel block in /etc/hosts to match
    /// server.toml. Requires root — runs via sudo
    Apply {
        /// `server.toml` to read the host list from. Defaults to the
        /// bougied-managed config
        #[arg(long, value_name = "PATH")]
        config: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServerTlsCommand {
    /// Fetch mkcert and install bougie's local CA
    Install,
    /// Uninstall bougie's local CA
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
    /// fragment to the durable, machine-local `conf.d-local/` (under
    /// `$BOUGIE_HOME`) without touching composer.json. Mix and match in
    /// one invocation
    Add {
        /// Extension names or `.so` paths (anything ending in `.so` is
        /// treated as a local file)
        args: Vec<String>,
        /// Skip the implicit `bougie sync` after the composer call
        #[arg(long)]
        no_sync: bool,
        #[command(flatten)]
        php: PhpPrefArgs,
    },
    /// Remove an extension dependency
    Remove {
        /// The extension(s) to remove
        names: Vec<String>,
        /// Skip the implicit `bougie sync` after the composer call
        #[arg(long)]
        no_sync: bool,
    },
    /// List available extensions
    List {
        /// Only show installed extensions
        #[arg(long)]
        only_installed: bool,
        /// Only show extensions advertised by the index
        #[arg(long)]
        only_available: bool,
        /// List all extension versions, including older releases
        #[arg(long)]
        all_versions: bool,
        /// List extensions for all platforms, not just the host's
        #[arg(long)]
        all_platforms: bool,
        /// Show the URLs of available extension downloads
        #[arg(long)]
        show_urls: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum PatchesCommand {
    /// Add a root patch rule from a URL or local file (the `composer
    /// require` of patches). The target package is inferred from the diff
    /// headers unless `--package` is given
    Add {
        /// An `http(s)://` URL or a local patch file path
        source: String,
        /// Target package (`vendor/pkg`); inferred from the diff if omitted
        #[arg(long, value_name = "VENDOR/PKG")]
        package: Option<String>,
        /// Human description (defaults to the URL basename / filename)
        #[arg(long)]
        description: Option<String>,
        /// Explicit `-pN` strip depth
        #[arg(long, value_name = "N")]
        depth: Option<usize>,
        /// Write into the external patches file rather than `extra.patches`
        #[arg(long = "to-file")]
        to_file: bool,
        /// Don't run `bougie sync` afterward
        #[arg(long = "no-sync")]
        no_sync: bool,
    },
    /// Author a patch from hand-edited `vendor/` files: diff an installed
    /// package against its originally-installed (pristine) contents and write
    /// a clean patch into the `patches/` directory, where bougie
    /// auto-discovers and re-applies it on the next `sync`
    Create {
        /// The installed package to diff (`vendor/package`)
        #[arg(value_name = "VENDOR/PACKAGE")]
        package: String,
        /// Where to write the patch (default:
        /// `<patches-dir>/<vendor>-<pkg>.patch`)
        #[arg(long, value_name = "PATH")]
        output: Option<String>,
        /// Print the diff to stdout instead of writing a file
        #[arg(long)]
        stdout: bool,
    },
    /// Show the resolved patch set, plus any unadopted dependency-declared
    /// patches (which bougie never applies automatically)
    List,
    /// Adopt dependency-declared patches into the root `composer.json`
    /// (the only way a dependency's patches ever apply)
    Import {
        /// Dependencies to import from (default: all that declare patches)
        packages: Vec<String>,
        /// Import from every dependency that declares patches
        #[arg(long)]
        all: bool,
        /// Write into the external patches file rather than `extra.patches`
        #[arg(long = "to-file")]
        to_file: bool,
    },
    /// Force a clean re-extract + re-apply for the named packages (or all):
    /// drops their recorded fingerprints, then syncs
    Repatch {
        /// Packages to repatch (default: all patched packages)
        packages: Vec<String>,
    },
    /// Rebuild `patches.lock.json` from current config: re-download remote
    /// patches and re-apply everything from pristine
    Relock,
    /// Diagnose patch configuration: unresolvable `patches/` files,
    /// `http://` URLs, missing checksums, unadopted dependency patches
    Doctor,
}

#[derive(Subcommand, Debug)]
pub enum PhpCommand {
    /// Install a new PHP version
    Install {
        /// The PHP version(s) to install (e.g. `8.3`, `8.3.12`, `8.3+zts`)
        requests: Vec<String>,
        /// Build flavor to install [possible values: nts, nts-debug, zts, zts-debug]
        #[arg(long)]
        flavor: Option<String>,
        /// Skip the entire baseline extension set; install only the bare
        /// Debian-aligned interpreter
        #[arg(long, conflicts_with = "without")]
        bare: bool,
        /// Skip a specific baseline extension. Repeatable: `--without opcache
        /// --without readline`. The named extensions must already be in the
        /// baseline set; use `bougie ext remove` after install for anything else
        #[arg(long, value_name = "EXT", action = clap::ArgAction::Append)]
        without: Vec<String>,
    },
    /// Remove a PHP version
    Uninstall {
        /// The PHP version(s) to uninstall
        #[arg(required = true)]
        requests: Vec<String>,
        /// Build flavor to uninstall [possible values: nts, nts-debug, zts, zts-debug]
        #[arg(long)]
        flavor: Option<String>,
    },
    /// List available PHP interpreters
    List {
        /// A PHP request to filter by
        request: Option<String>,
        /// Only show installed PHP versions
        #[arg(long)]
        only_installed: bool,
        /// Only show PHP versions available for download
        #[arg(long)]
        only_available: bool,
        /// List all PHP versions, including older patch versions
        #[arg(long)]
        all_versions: bool,
        /// List PHP downloads for all platforms
        #[arg(long)]
        all_platforms: bool,
        /// List PHP downloads for all architectures
        #[arg(long)]
        all_arches: bool,
        /// Show the URLs of available PHP downloads
        #[arg(long)]
        show_urls: bool,
    },
    /// Search for a PHP interpreter
    Find {
        /// A PHP request to search for
        request: Option<String>,
    },
    /// Pin the project's PHP version
    Pin {
        /// The PHP version to pin
        request: String,
        /// Write the pin to `bougie.toml` (creating it if needed)
        #[arg(long, conflicts_with = "composer")]
        toml: bool,
        /// Write the pin to `composer.json`'s `require.php`
        #[arg(long, conflicts_with = "toml")]
        composer: bool,
    },
    /// Refresh installed interpreters to the latest published patch
    Upgrade {
        /// The PHP minor version(s) to upgrade (e.g. `8.3`)
        minor: Option<String>,
    },
    /// Show the PHP interpreter installation directory
    Dir,
}

#[derive(Subcommand, Debug)]
pub enum NodeCommand {
    /// Install a Node.js version from nodejs.org
    Install {
        /// The Node version(s) to install (e.g. `latest`, `lts`, `20`,
        /// `20.11`, `20.11.0`). Defaults to `latest`
        requests: Vec<String>,
    },
    /// Remove an installed Node.js version
    Uninstall {
        /// The Node version(s) to uninstall (exact `20.11.0`)
        #[arg(required = true)]
        requests: Vec<String>,
    },
    /// List installed Node.js versions
    List,
    /// Resolve a request and show the version + download URL it maps to,
    /// without installing
    Find {
        /// A Node request to resolve (e.g. `lts`, `20`). Defaults to `latest`
        request: Option<String>,
    },
    /// Show the Node.js installation directory
    Dir,
}

#[derive(Subcommand, Debug)]
pub enum ComposerCommand {
    /// Install `vendor/` from `composer.lock`
    ///
    /// Reads `composer.json` + `composer.lock` in the working directory,
    /// content-hash-verifies the lock, parallel-downloads dists into
    /// `vendor/`, and emits `vendor/autoload.php`
    Install {
        /// Run the install in this directory instead of CWD
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Skip dev-only packages and dev autoload entries
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Fail if composer.lock is out of sync with composer.json.
        /// Currently a no-op — the install already errors on
        /// content-hash mismatch by default
        #[arg(long = "frozen")]
        frozen: bool,
        /// Verify the lock is internally consistent (content-hash,
        /// requires, transitives) and exit. Doesn't touch `vendor/`
        /// or run the autoloader. CI-friendly read-only check
        #[arg(long = "lock-verify")]
        lock_verify: bool,
        /// Ignore all platform requirements (php, ext-*, lib-*). bougie
        /// does not enforce platform requirements yet
        #[arg(long = "ignore-platform-reqs")]
        ignore_platform_reqs: bool,
        /// Ignore a specific platform requirement
        #[arg(long = "ignore-platform-req", value_name = "REQ")]
        ignore_platform_req: Vec<String>,
        /// Run composer.json root scripts, overriding `[scripts] run`
        /// in bougie.toml. Off by default (opt-in)
        #[arg(long, conflicts_with = "no_scripts")]
        scripts: bool,
        /// Skip composer.json root scripts, overriding `[scripts] run
        /// = true` in bougie.toml
        #[arg(long = "no-scripts")]
        no_scripts: bool,
        /// Apply patches, overriding `[patches] enable`. On by default
        /// when patches are declared
        #[arg(long, conflicts_with = "no_patches")]
        patches: bool,
        /// Skip native patch application for this install
        #[arg(long = "no-patches")]
        no_patches: bool,
    },
    /// Update dependencies and `composer.lock`
    ///
    /// Re-resolve the dependency graph, write a fresh `composer.lock`,
    /// and install the result into `vendor/`. With no packages the whole
    /// graph re-resolves; naming packages does a partial update, leaving
    /// every other locked package pinned. `--no-install` stops after
    /// writing the lock; `--dry-run` previews without writing. Aliased to
    /// `upgrade` / `u`
    #[command(visible_alias = "upgrade", alias = "u")]
    Update {
        /// Packages to update (`vendor/name`). When given, only these
        /// packages re-resolve; every other package stays pinned to its
        /// `composer.lock` version. With no packages, the whole graph
        /// re-resolves from scratch
        #[arg(value_name = "PACKAGES")]
        packages: Vec<String>,
        /// Write the lock but don't install into `vendor/`
        #[arg(long = "no-install")]
        no_install: bool,
        /// Also update the named packages' dependencies (`-w`)
        #[arg(short = 'w', long = "with-dependencies")]
        with_dependencies: bool,
        /// Also update all of the named packages' dependencies, including
        /// ones shared with other packages (`-W`)
        #[arg(short = 'W', long = "with-all-dependencies")]
        with_all_dependencies: bool,
        /// Run the update in this directory instead of CWD
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Skip dev-only root requires when resolving
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Version-preference policy when resolving
        #[arg(long = "resolution", value_name = "STRATEGY", default_value = "highest")]
        resolution: ResolutionStrategy,
        /// Prefer the lowest matching versions. Equivalent to
        /// `--resolution lowest`; when set it overrides `--resolution`
        #[arg(long = "prefer-lowest")]
        prefer_lowest: bool,
        /// Resolve and print the solution without writing
        /// `composer.lock` or touching `vendor/`. Without this flag,
        /// `update` writes a fresh `composer.lock`
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Ignore all platform requirements (php, ext-*, lib-*). bougie
        /// does not enforce platform requirements yet
        #[arg(long = "ignore-platform-reqs")]
        ignore_platform_reqs: bool,
        /// Ignore a specific platform requirement
        #[arg(long = "ignore-platform-req", value_name = "REQ")]
        ignore_platform_req: Vec<String>,
    },
    /// Validate composer.json structure and contents
    Validate {
        /// Run in this directory instead of CWD
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Return non-zero exit code for warnings too
        #[arg(long)]
        strict: bool,
        /// Skip lock file freshness check
        #[arg(long = "no-check-lock")]
        no_check_lock: bool,
        /// Skip publish-only checks (name casing, required fields)
        #[arg(long = "no-check-publish")]
        no_check_publish: bool,
        /// Skip unbound/exact version constraint warnings
        #[arg(long = "no-check-all")]
        no_check_all: bool,
        /// Also validate installed dependencies' composer.json files
        #[arg(long = "with-dependencies")]
        with_dependencies: bool,
        /// Force lock file checking even when `config.lock` is false
        #[arg(long = "check-lock")]
        check_lock: bool,
    },
    /// Regenerate the autoloader files
    ///
    /// Regenerate `vendor/composer/autoload_*.php` against the current
    /// `composer.lock`. Aliased to `dump-autoload`
    #[command(alias = "dump-autoload")]
    DumpAutoloader {
        /// Optimize the classmap (`--optimize` / `-o`)
        #[arg(short = 'o', long = "optimize", alias = "optimize-autoloader")]
        optimize: bool,
        /// Emit the classmap-authoritative static loader
        /// (`--classmap-authoritative` / `-a`). Implies `--optimize`
        #[arg(short = 'a', long = "classmap-authoritative")]
        classmap_authoritative: bool,
        /// Skip dev autoload entries (`--no-dev`)
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Emit the `APCu` loader bootstrap (`--apcu-autoloader`)
        #[arg(long = "apcu-autoloader")]
        apcu_autoloader: bool,
        /// Explicit `APCu` prefix; implies `--apcu-autoloader`
        #[arg(long = "apcu-autoloader-prefix", value_name = "PREFIX")]
        apcu_prefix: Option<String>,
        /// Override the `ComposerAutoloaderInit<X>` class suffix —
        /// otherwise the value from `composer.json`'s
        /// `config.autoloader-suffix`, or the `composer.lock`
        /// content-hash
        #[arg(long = "autoloader-suffix", value_name = "SUFFIX")]
        autoloader_suffix: Option<String>,
        /// Run the dump in this directory instead of the current one
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Add packages to `composer.json` and install them
    ///
    /// Add one or more packages to `composer.json` `require` (or
    /// `require-dev`), re-resolve `composer.lock`, and install them. A
    /// bare `vendor/pkg` resolves the latest stable and writes a caret
    /// (`^X.Y`) constraint; set an explicit constraint with
    /// `vendor/pkg:^1.0`, `vendor/pkg=^1.0`, or a trailing argument
    /// (`vendor/pkg ^1.0`)
    Require {
        /// Packages to require (`vendor/pkg` or `vendor/pkg:<constraint>`)
        #[arg(value_name = "PACKAGES", required = true)]
        packages: Vec<String>,
        /// Add to `require-dev` instead of `require`
        #[arg(long = "dev")]
        dev: bool,
        /// Edit `composer.json` only — don't re-resolve `composer.lock`
        /// or touch `vendor/`
        #[arg(long = "no-update")]
        no_update: bool,
        /// Re-resolve and write `composer.lock` but don't install into
        /// `vendor/`
        #[arg(long = "no-install")]
        no_install: bool,
        /// Also update the new packages' dependencies (`-w`)
        #[arg(short = 'w', long = "with-dependencies")]
        with_dependencies: bool,
        /// Also update all dependencies, including shared ones (`-W`)
        #[arg(short = 'W', long = "with-all-dependencies")]
        with_all_dependencies: bool,
        /// Prefer the lowest matching versions when resolving
        #[arg(long = "prefer-lowest")]
        prefer_lowest: bool,
        /// Ignore all platform requirements (php, ext-*, lib-*)
        #[arg(long = "ignore-platform-reqs")]
        ignore_platform_reqs: bool,
        /// Ignore a specific platform requirement
        #[arg(long = "ignore-platform-req", value_name = "REQ")]
        ignore_platform_req: Vec<String>,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Resolve and report what would change without writing
        /// `composer.json`, `composer.lock`, or `vendor/`
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Remove packages and uninstall them from `vendor/`
    ///
    /// Remove one or more packages from `composer.json`, re-resolve
    /// `composer.lock`, and uninstall them from `vendor/`
    Remove {
        /// Packages to remove (`vendor/name`)
        #[arg(value_name = "PACKAGES", required = true)]
        packages: Vec<String>,
        /// Remove from `require-dev` instead of `require`
        #[arg(long = "dev")]
        dev: bool,
        /// Edit `composer.json` only — don't re-resolve or touch
        /// `vendor/`
        #[arg(long = "no-update")]
        no_update: bool,
        /// Re-resolve and write `composer.lock` but don't touch
        /// `vendor/`
        #[arg(long = "no-install")]
        no_install: bool,
        /// Skip dev-only packages when resolving
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Ignore all platform requirements (php, ext-*, lib-*)
        #[arg(long = "ignore-platform-reqs")]
        ignore_platform_reqs: bool,
        /// Ignore a specific platform requirement
        #[arg(long = "ignore-platform-req", value_name = "REQ")]
        ignore_platform_req: Vec<String>,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Resolve and report what would change without writing
        /// `composer.json`, `composer.lock`, or `vendor/`
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// List installed packages, or show details for one
    ///
    /// Reads the project's `composer.lock`. Aliases `info`, `list`
    #[command(alias = "info", alias = "list")]
    Show {
        /// A single `vendor/name` to show details for. With no argument,
        /// every installed package is listed
        #[arg(value_name = "PACKAGE")]
        package: Option<String>,
        /// Render the dependency tree (`--tree` / `-t`)
        #[arg(short = 't', long = "tree")]
        tree: bool,
        /// Only the project's direct dependencies (`--direct` / `-D`)
        #[arg(short = 'D', long = "direct")]
        direct: bool,
        /// Only platform packages — php, ext-*, lib-* (`--platform` / `-p`)
        #[arg(short = 'p', long = "platform")]
        platform: bool,
        /// Show the root package's own info (`--self` / `-s`)
        #[arg(short = 's', long = "self")]
        self_: bool,
        /// Print package names only (`--name-only` / `-N`)
        #[arg(short = 'N', long = "name-only")]
        name_only: bool,
        /// Show each package's install path (`--path` / `-P`)
        #[arg(short = 'P', long = "path")]
        path: bool,
        /// Also fetch and show the latest available version
        /// (`--latest` / `-l`)
        #[arg(short = 'l', long = "latest")]
        latest: bool,
        /// Only packages with a newer version available
        /// (`--outdated` / `-o`). Implies `--latest`
        #[arg(short = 'o', long = "outdated")]
        outdated: bool,
        /// Skip dev dependencies (`--no-dev`)
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Show which packages depend on a given package
    ///
    /// Shows why a package is installed. Alias `depends`
    #[command(alias = "depends")]
    Why {
        /// The package to explain
        #[arg(value_name = "PACKAGE", required = true)]
        package: String,
        /// Recurse through the dependency chain (`--recursive` / `-r`)
        #[arg(short = 'r', long = "recursive")]
        recursive: bool,
        /// Render the full dependency-of tree (`--tree` / `-t`)
        #[arg(short = 't', long = "tree")]
        tree: bool,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Show what prevents a package from being installed
    ///
    /// Reports the conflicting requirements for a package, optionally at
    /// a given version. Alias `prohibits`
    #[command(name = "why-not", alias = "prohibits")]
    WhyNot {
        /// The package to test
        #[arg(value_name = "PACKAGE", required = true)]
        package: String,
        /// The version (or constraint) to test against. Defaults to `*`
        #[arg(value_name = "VERSION")]
        version: Option<String>,
        /// Recurse through the dependency chain (`--recursive` / `-r`)
        #[arg(short = 'r', long = "recursive")]
        recursive: bool,
        /// Render the full tree (`--tree` / `-t`)
        #[arg(short = 't', long = "tree")]
        tree: bool,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// List installed packages with a newer version available
    ///
    /// Use the global `--format json` for JSON output
    Outdated {
        /// Optional `vendor/name` filters; with none, all packages are
        /// considered
        #[arg(value_name = "PACKAGES")]
        packages: Vec<String>,
        /// Only the project's direct dependencies (`--direct` / `-D`)
        #[arg(short = 'D', long = "direct")]
        direct: bool,
        /// Only show packages with a new major version (`--major-only`)
        #[arg(long = "major-only")]
        major_only: bool,
        /// Only show packages with a new minor version (`--minor-only`)
        #[arg(long = "minor-only")]
        minor_only: bool,
        /// Only show packages with a new patch version (`--patch-only`)
        #[arg(long = "patch-only")]
        patch_only: bool,
        /// Skip dev dependencies (`--no-dev`)
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Exit non-zero if any package is outdated (`--strict`)
        #[arg(long = "strict")]
        strict: bool,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Check installed packages for security advisories
    ///
    /// Checks against the Packagist security-advisories database. Exits
    /// non-zero when advisories are found. Use the global `--format json`
    /// for JSON
    Audit {
        /// Skip dev dependencies (`--no-dev`)
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// How to treat abandoned packages. Detection is not yet wired
        /// up
        #[arg(long = "abandoned", value_enum, default_value = "report")]
        abandoned: AbandonedHandling,
        /// Audit the locked set. bougie always reads `composer.lock`, so
        /// this is the default
        #[arg(long = "locked")]
        locked: bool,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// List the license of every installed package
    ///
    /// Use the global `--format json` for JSON
    Licenses {
        /// Skip dev dependencies (`--no-dev`)
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Report packages that look locally modified
    ///
    /// bougie installs from dist archives, so for the common case this
    /// reports "no local changes"
    Status {
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Show funding information for installed packages
    ///
    /// Grouped by vendor. Use `--format json` for JSON
    Fund {
        /// Skip dev dependencies (`--no-dev`)
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Run in this directory instead of CWD (`-d`)
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
    },
    /// Catch-all for any composer subcommand bougie does not implement
    /// natively (`create-project`, `archive`, `bump`, `global`, …). These
    /// return an error pointing at `bougie tool install composer/composer`
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

/// How `composer audit` treats abandoned packages.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbandonedHandling {
    /// Ignore abandoned packages entirely
    Ignore,
    /// Report abandoned packages but don't fail on them
    Report,
    /// Treat abandoned packages as an audit failure
    Fail,
}

#[derive(Subcommand, Debug)]
pub enum CacheCommand {
    /// Wipe the full cache
    Clean,
    /// Remove unneeded library files
    Prune {
        /// Show what would be pruned without removing anything
        #[arg(long)]
        dry_run: bool,
        /// Also remove tracked projects that no longer exist on disk
        #[arg(long)]
        prune_projects: bool,
    },
    /// Show the location of the cache directory
    Dir,
    /// Show the cache size
    Size,
}

#[derive(Subcommand, Debug)]
pub enum TelemetryCommand {
    /// Show the telemetry mode, where it came from, and what's spooled
    Status,
    /// Enable telemetry: record consent and mint the anonymous install id
    On,
    /// Disable telemetry entirely (no local recording, nothing sent)
    Off,
    /// Record events locally but never upload them
    Local,
    /// Print locally spooled events — exactly what would be uploaded
    Log {
        /// Show at most the last N events (0 = all)
        #[arg(short = 'n', long = "lines", default_value_t = 20)]
        lines: usize,
    },
    /// Rotate the anonymous install id and purge all spooled events
    Reset,
}

#[derive(Subcommand, Debug)]
pub enum ToolCommand {
    /// Install a tool. Pass `<vendor>/<name>` optionally followed by
    /// `@<constraint>` (e.g. `phpstan/phpstan@^1.10`)
    Install {
        /// Composer package identifier, optionally with `@<constraint>`
        package: String,
        /// Pin the tool to a specific PHP. Accepts a version (`8.3`,
        /// `8.3.12`) or a constraint (`~8.3`, `>=8.2,<8.4`). When the
        /// requested PHP isn't installed, bougie installs it
        /// automatically. Defaults to the highest installed NTS PHP
        #[arg(long, value_name = "VER")]
        php: Option<String>,
        /// Additional Composer package (`vendor/name[@<constraint>]`)
        /// or PHP extension (`intl`, `redis`) to install alongside the
        /// tool. May be passed multiple times
        #[arg(long, value_name = "PKG_OR_EXT")]
        with: Vec<String>,
        /// Overwrite an existing executable at the bin-dir path
        #[arg(long)]
        force: bool,
    },
    /// Remove an installed tool by its `<vendor>/<name>` identifier
    Uninstall {
        /// Composer package identifier
        package: String,
    },
    /// Add an extra composer package or PHP extension to an
    /// installed tool. Re-resolves the tool's lock and updates the
    /// vendor tree in place
    Inject {
        /// Composer package identifier of the tool
        package: String,
        /// Extra to add (`vendor/name[@<constraint>]` for composer
        /// packages, bare name for PHP extensions). Repeatable
        #[arg(long, value_name = "PKG_OR_EXT", required = true)]
        with: Vec<String>,
    },
    /// Remove an extra previously added via `--with` / `inject`
    Uninject {
        /// Composer package identifier of the tool
        package: String,
        /// Extra to remove. Repeatable
        #[arg(long, value_name = "PKG_OR_EXT", required = true)]
        with: Vec<String>,
    },
    /// List installed tools
    List,
    /// Print a tool's install directory, or the tools root if no
    /// package is given
    Dir {
        /// Composer package identifier; omit to print the tools root
        package: Option<String>,
    },
    /// Run an installed-or-cached tool one-off. Reuses an existing
    /// persistent install if `(package, constraint, php, with)` match
    /// exactly; otherwise materialises into the ephemeral cache.
    ///
    /// `bgx` is provided as a convenient alias for `bougie tool run`;
    /// their behavior is identical
    #[command(
        override_usage = "bougie tool run [OPTIONS] <PACKAGE> [ARGS]...",
        after_help = "Use `bgx` as a shortcut for `bougie tool run`.\n\n\
                      Use `bougie help tool run` for more details",
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
        about = "Run a tool from a PHP package",
        long_about = None,
        after_help = "Use `bougie help tool run` for more details",
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
    /// wipe and rebuild from scratch (recovery for broken state)
    Upgrade {
        /// Composer package identifier. Required unless `--all`
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        package: Option<String>,
        /// Upgrade every installed tool
        #[arg(long)]
        all: bool,
        /// Wipe the tool dir + every entrypoint symlink and reinstall
        /// from scratch using the receipt's pinned `(package,
        /// constraint, php_version, with, extensions)` tuple
        #[arg(long)]
        reinstall: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SelfCommand {
    /// Update bougie
    Update {
        /// Update even when bougie can't confirm it installed this
        /// binary. By default `self update` only touches a binary that
        /// bougie's own installer placed (per the install receipt);
        /// copies from a package manager, cargo, or nix are left for
        /// that tool to update. Pass `--force` only if you know this
        /// copy came from bougie's installer
        #[arg(long)]
        force: bool,
    },
    /// Show bougie's version
    Version {
        /// Only show the version
        #[arg(long)]
        short: bool,
    },
}

#[derive(Args, Debug)]
pub struct ToolRunArgs {
    /// Pin the tool to a specific PHP for this run
    #[arg(long, value_name = "VER")]
    pub php: Option<String>,
    /// Extra composer package or PHP extension, same shape as
    /// `tool install --with`. Repeatable
    #[arg(long, value_name = "PKG_OR_EXT")]
    pub with: Vec<String>,
    /// Ignore the surrounding PHP project. By default a project's
    /// PHP version and required extensions are applied to the run
    /// (tool requirements win on conflict)
    #[arg(long)]
    pub no_project: bool,
    /// The tool's Composer package (optionally `@<constraint>`) followed
    /// by the arguments to forward to it. bougie's own options must come
    /// *before* the package; everything from the package onward is passed
    /// to the tool verbatim, so no `--` separator is needed
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        required = true,
        value_name = "PACKAGE"
    )]
    pub command: Vec<std::ffi::OsString>,
}

/// Args for the hidden `bgx` alias. Wraps [`ToolRunArgs`] verbatim so
/// the two variants share their entire surface; the wrapper exists
/// only so clap renders help / errors with `bgx` as the program name.
#[derive(Args, Debug)]
pub struct BgxArgs {
    #[command(flatten)]
    pub tool_run: ToolRunArgs,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cmd(argv: &[&str]) -> Command {
        Cli::try_parse_from(argv).expect("parse").command
    }

    #[test]
    fn start_is_its_own_verb() {
        assert!(matches!(cmd(&["bougie", "start"]), Command::Start { .. }));
        assert!(matches!(
            cmd(&["bougie", "start", "--no-sync", "--dry-run"]),
            Command::Start { no_sync: true, dry_run: true, .. }
        ));
    }

    #[test]
    fn stop_takes_names_and_purge() {
        let Command::Stop { names, purge } = cmd(&["bougie", "stop", "redis", "--purge"]) else {
            panic!("expected stop");
        };
        assert_eq!(names, ["redis"]);
        assert!(purge);
    }

    #[test]
    fn up_down_live_under_service() {
        assert!(matches!(
            cmd(&["bougie", "service", "up", "redis", "-d"]),
            Command::Service(ServiceCommand::Up { detach: true, .. })
        ));
        assert!(matches!(
            cmd(&["bougie", "service", "down", "--purge"]),
            Command::Service(ServiceCommand::Down { purge: true, .. })
        ));
    }

    #[test]
    fn services_is_an_alias_for_service() {
        // The pre-rename spelling must keep parsing to the same command.
        assert!(matches!(
            cmd(&["bougie", "services", "up", "redis", "-d"]),
            Command::Service(ServiceCommand::Up { detach: true, .. })
        ));
        assert!(matches!(
            cmd(&["bougie", "services", "catalog"]),
            Command::Service(ServiceCommand::Catalog)
        ));
    }

    #[test]
    fn top_level_up_down_are_gone() {
        // The deprecated top-level aliases were removed; `up`/`down` only
        // exist under `service` now.
        assert!(Cli::try_parse_from(["bougie", "up"]).is_err());
        assert!(Cli::try_parse_from(["bougie", "down"]).is_err());
    }

    #[test]
    fn server_detach_flag() {
        for argv in [
            &["bougie", "server", "-d"][..],
            &["bougie", "server", "--detach"][..],
        ] {
            let Command::Server(args) = cmd(argv) else {
                panic!("expected server for {argv:?}");
            };
            assert!(args.serve.detach, "detach should be set for {argv:?}");
        }
        // The old `--no-attach` spelling is gone.
        assert!(Cli::try_parse_from(["bougie", "server", "--no-attach"]).is_err());
    }

    #[test]
    fn make_no_longer_aliases_start() {
        // `start` is no longer a clap alias of `make`; it's the
        // first-class verb above. `bougie make start` is just `make`
        // with the literal task `start`.
        let Command::Make { task, .. } = cmd(&["bougie", "make", "start"]) else {
            panic!("expected make");
        };
        assert_eq!(task.as_deref(), Some("start"));

        // Bare `bougie make` parses with no task; the dispatcher turns
        // that into a task listing.
        let Command::Make { task, .. } = cmd(&["bougie", "make"]) else {
            panic!("expected make");
        };
        assert_eq!(task, None);
    }
}
