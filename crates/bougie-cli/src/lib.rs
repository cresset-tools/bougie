use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Parser, Subcommand};

const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Magenta.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Magenta.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::BrightMagenta.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::BrightMagenta.on_default())
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
    .valid(AnsiColor::Green.on_default())
    .invalid(AnsiColor::Yellow.on_default().effects(Effects::BOLD));

#[derive(Parser, Debug)]
#[command(name = "bougie", version, about, long_about = None, styles = HELP_STYLES)]
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

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    #[value(name = "json-v1")]
    JsonV1,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a new project.
    Init {
        /// Place bougie configuration in a bougie.toml file.
        #[arg(long)]
        toml: bool,
    },

    /// Manage PHP extensions.
    #[command(subcommand)]
    Ext(ExtCommand),

    /// Install everything the project requires.
    Sync {
        /// Don't try to download anything, this will fail if there are uncached packages.
        #[arg(long)]
        offline: bool,
        /// Show the plan, change nothing on disk.
        #[arg(long)]
        dry_run: bool,
    },

    /// Start the project's declared services (or every service in
    /// `names`) and provision the project's tenant in each. Equivalent
    /// to the former `bougie services up` — promoted to a top-level
    /// verb because it's the most common project-startup step.
    Up {
        /// Service names to bring up. Empty = every declared service.
        names: Vec<String>,
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
        /// Command and arguments. `--` separator is optional.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        argv: Vec<String>,
    },

    /// Manage PHP interpreters.
    #[command(subcommand)]
    Php(PhpCommand),

    /// Manage Composer installs.
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

    /// Run the bougie development HTTP server. With no subcommand, runs
    /// the foreground server. See SERVER.md.
    #[command(subcommand)]
    Server(ServerCommand),

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
    /// Tail (and optionally follow) a service's log.
    Logs {
        /// Service name.
        name: String,
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

#[derive(Subcommand, Debug)]
pub enum ServerCommand {
    /// Run the server in the foreground. `--config` is mandatory: the
    /// CLI no longer ships an `add`/`remove` mutator pair against an
    /// XDG-default `server.toml`, so every invocation explicitly names
    /// the file to read. The bougied-managed path
    /// (`bougie services up server`) supplies its own service-scoped
    /// `server.toml`; users running the server by hand point at one
    /// they wrote themselves.
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
    /// List hosts configured in a `server.toml`. `--config` is
    /// mandatory: there is no XDG-default fallback, every invocation
    /// names the file to read.
    List {
        /// `server.toml` path. Required.
        #[arg(long, value_name = "PATH")]
        config: std::path::PathBuf,
    },
    /// Manage `/etc/hosts` overrides (phase 5).
    #[command(subcommand)]
    Hosts(ServerHostsCommand),
    /// Manage local TLS via mkcert (phase 7).
    #[command(subcommand)]
    Tls(ServerTlsCommand),
}

#[derive(Subcommand, Debug)]
pub enum ServerHostsCommand {
    /// Rewrite the bougie sentinel block in /etc/hosts to match
    /// server.toml. Requires root — runs via sudo.
    Apply {
        /// `server.toml` path. Required: there is no XDG-default
        /// fallback, and a sudo invocation would in any case strip the
        /// env that one used to come from.
        #[arg(long, value_name = "PATH")]
        config: std::path::PathBuf,
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
    /// `vendor/`, and emits `vendor/autoload.php`. Replaces today's
    /// binary-management `install <version>` — use `bougie composer
    /// fetch <version>` for that.
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
    },
    /// Resolve the project's dependency graph from scratch (read
    /// `composer.json`, ignore any existing `composer.lock`) and
    /// produce a fresh solution. Currently only the read-only
    /// `--dry-run` mode is implemented; writing `composer.lock` is
    /// a follow-up.
    Update {
        /// Run the update in this directory instead of CWD.
        /// Mirrors Composer's `--working-dir` / `-d`.
        #[arg(short = 'd', long = "working-dir", value_name = "DIR")]
        working_dir: Option<std::path::PathBuf>,
        /// Skip dev-only root requires when resolving.
        #[arg(long = "no-dev")]
        no_dev: bool,
        /// Resolve and print the solution without writing
        /// `composer.lock` or touching `vendor/`. Currently the only
        /// supported mode — without this flag the command refuses
        /// to run.
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
    /// Download and install a Composer phar version into
    /// `$BOUGIE_LOCAL/composer/<version>/`. The verb formerly known
    /// as `install <version>`.
    Fetch {
        /// The Composer version to install (exact, partial, or channel).
        request: Option<String>,
    },
    /// Remove a Composer version.
    Uninstall {
        /// The Composer version to uninstall.
        request: String,
    },
    /// List installed and available Composer versions.
    List,
    /// Print the path of a Composer phar.
    Find {
        /// The Composer version to locate.
        request: Option<String>,
    },
    /// Pin the project's Composer version.
    Pin {
        /// The Composer version to pin (exact, partial, or channel).
        request: String,
        /// Write the pin to `bougie.toml` (creating it if needed).
        #[arg(long, conflicts_with = "composer")]
        toml: bool,
        /// Write the pin to `composer.json`'s `extra.bougie`.
        #[arg(long, conflicts_with = "toml")]
        composer: bool,
    },
    /// Show the Composer install directory.
    Dir,
    /// Refresh the stable + preview Composer channels to the latest.
    Upgrade,
    /// Regenerate `vendor/composer/autoload_*.php` against the current
    /// `composer.lock`. Drop-in for `composer dump-autoload`; output
    /// is byte-equivalent to Composer 2.8.12 with the same flags. Aliased
    /// to `dump-autoload` for users coming from Composer muscle-memory.
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
    Update,
    /// Show bougie's version.
    Version {
        /// Only show the version.
        #[arg(long)]
        short: bool,
    },
}
