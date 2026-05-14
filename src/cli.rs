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

    /// Extract a single scalar field from the result.
    #[arg(long, global = true, value_name = "PATH")]
    pub field: Option<String>,
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

    /// Run a command in the project environment.
    Run {
        /// Add a temporary extension for this invocation.
        #[arg(long, value_name = "EXT=VER")]
        with: Vec<String>,
        /// Skip the implicit `bougie sync` before running.
        #[arg(long)]
        no_sync: bool,
        /// Layer the server's debug overlay (`.bougie/conf.d-debug/`)
        /// into PHP_INI_SCAN_DIR and set `XDEBUG_SESSION=1` for the
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
}

#[derive(Subcommand, Debug)]
pub enum ServicesCommand {
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
    /// Run the server in the foreground.
    Run {
        /// Alternate server.toml path.
        #[arg(long, value_name = "PATH")]
        config: Option<std::path::PathBuf>,
        /// CLI override of `[server].listen` (e.g. `127.0.0.1:7080`).
        #[arg(long, value_name = "ADDR")]
        listen: Option<String>,
        /// CLI override of `[server].log_format`.
        #[arg(long, value_name = "FMT")]
        log_format: Option<String>,
    },
    /// Add a `[[host]]` block to server.toml.
    Add {
        /// Hostname (e.g. `myapp.bougie.run`).
        hostname: String,
        /// Project root. When omitted, bougie walks up from cwd
        /// looking for `composer.json`, `bougie.toml`, or `.bougie/`
        /// and uses the first match.
        project: Option<std::path::PathBuf>,
        /// Web root, relative to the project (default `.`).
        #[arg(long)]
        root: Option<String>,
    },
    /// Remove a `[[host]]` block by hostname.
    Remove {
        /// Hostname to remove (matches `hostname` or any `[[host.alias]]`).
        hostname: String,
    },
    /// List configured hosts.
    List,
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
    ///
    /// With `[server].manage_etc_hosts = true`, `bougie server
    /// add/remove` invoke this automatically after every mutation.
    Apply {
        /// Alternate server.toml path. Required when invoking via
        /// sudo because sudo strips XDG_CONFIG_HOME by default.
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
    /// Add an extension dependency.
    Add {
        /// The extension(s) to add, optionally with `@<version>` pins.
        names: Vec<String>,
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
        /// Skip the baseline extension set; install only the Debian-aligned core.
        #[arg(long, conflicts_with = "baseline_only")]
        no_baseline: bool,
        /// Install only the listed baseline extensions (comma-separated).
        #[arg(long, value_name = "EXT[,EXT…]")]
        baseline_only: Option<String>,
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
    /// Install a Composer version.
    Install {
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
