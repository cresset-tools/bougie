use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Parser, Subcommand};
use eyre::Result;
use std::process::ExitCode;

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
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Suppress non-error output.
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Verbose output.
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Output format.
    #[arg(long, global = true, default_value = "text")]
    format: OutputFormat,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum OutputFormat {
    Text,
    #[value(name = "json-v1")]
    JsonV1,
}

#[derive(Subcommand, Debug)]
enum Command {
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
        /// Command and arguments. Must follow `--`.
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },

    /// Manage PHP interpreters.
    #[command(subcommand)]
    Php(PhpCommand),

    /// Manage bougie's cache.
    #[command(subcommand)]
    Cache(CacheCommand),

    /// Manage the bougie binary itself.
    #[command(subcommand)]
    #[command(name = "self")]
    SelfCmd(SelfCommand),
}

#[derive(Subcommand, Debug)]
enum ExtCommand {
    /// Add an extension dependency.
    Add { names: Vec<String> },
    /// Remove an extension dependency.
    Remove { names: Vec<String> },
    /// List available extensions.
    List {
        #[arg(long)]
        only_installed: bool,
        #[arg(long)]
        only_available: bool,
        #[arg(long)]
        all_versions: bool,
        #[arg(long)]
        all_platforms: bool,
        #[arg(long)]
        show_urls: bool,
    },
}

#[derive(Subcommand, Debug)]
enum PhpCommand {
    /// Install a new PHP version.
    Install {
        request: Option<String>,
        #[arg(long)]
        flavor: Option<String>,
    },
    /// Remove a PHP version.
    Uninstall {
        request: String,
        #[arg(long)]
        flavor: Option<String>,
    },
    /// List available PHP interpreters.
    List {
        request: Option<String>,
        #[arg(long)]
        only_installed: bool,
        #[arg(long)]
        only_available: bool,
        #[arg(long)]
        all_versions: bool,
        #[arg(long)]
        all_platforms: bool,
        #[arg(long)]
        all_arches: bool,
        #[arg(long)]
        show_urls: bool,
    },
    /// Search for a PHP interpreter.
    Find { request: Option<String> },
    /// Pin the project's PHP version.
    Pin {
        request: String,
        #[arg(long, conflicts_with = "composer")]
        toml: bool,
        #[arg(long, conflicts_with = "toml")]
        composer: bool,
    },
    /// Refresh installed interpreters to the latest published patch.
    Upgrade { minor: Option<String> },
    /// Show the PHP interpreter installation directory.
    Dir,
}

#[derive(Subcommand, Debug)]
enum CacheCommand {
    /// Wipe the full cache.
    Clean,
    /// Remove unneeded library files.
    Prune {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        prune_projects: bool,
    },
    /// Show the location of the cache directory.
    Dir,
    /// Show the cache size.
    Size,
}

#[derive(Subcommand, Debug)]
enum SelfCommand {
    /// Update bougie
    Update,
    /// Display bougie's version
    Version {
        /// Only show the version.
        #[arg(long)]
        short: bool,
    },
}

fn main() -> Result<ExitCode> {
    color_eyre::install()?;
    let cli = Cli::parse();

    match cli.command {
        Command::Init { .. }
        | Command::Ext(_)
        | Command::Sync { .. }
        | Command::Run { .. }
        | Command::Php(_)
        | Command::Cache(_)
        | Command::SelfCmd(_) => {
            eprintln!("not yet implemented: {:?}", cli.command);
            Ok(ExitCode::from(1))
        }
    }
}
