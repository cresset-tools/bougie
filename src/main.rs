use clap::{Parser, Subcommand};
use eyre::Result;
use std::process::ExitCode;

/// `bougie`: uv for PHP. Manages relocatable PHP installs and per-project
/// extension sets. See CLI.md in the spec repo for the full surface.
#[derive(Parser, Debug)]
#[command(name = "bougie", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Suppress non-error output.
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Verbose output (resolved URLs, cache hits, timings).
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Output format. See spec §9 for the schema contract.
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
    /// Create a new project (`composer.json`, `.bougie/` skeleton).
    Init {
        /// Also create a separate `bougie.toml` instead of using composer.json's `extra.bougie`.
        #[arg(long)]
        toml: bool,
    },

    /// Manage PHP extensions.
    #[command(subcommand)]
    Ext(ExtCommand),

    /// Install everything the project requires.
    Sync {
        /// Refuse network; use only cached state.
        #[arg(long)]
        offline: bool,
        /// Print the plan, change nothing on disk.
        #[arg(long)]
        dry_run: bool,
    },

    /// Run a command in the project environment.
    Run {
        /// Skip the implicit `bougie sync` first.
        #[arg(long)]
        no_sync: bool,
        /// Add an ephemeral extension for this invocation.
        #[arg(long, value_name = "EXT=VER")]
        with: Vec<String>,
        /// Command and arguments. Must follow `--`.
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },

    /// Manage PHP interpreters.
    #[command(subcommand)]
    Php(PhpCommand),

    /// Manage the cache and content-addressed store.
    #[command(subcommand)]
    Cache(CacheCommand),

    /// Manage the bougie binary itself.
    #[command(subcommand)]
    #[command(name = "self")]
    SelfCmd(SelfCommand),
}

#[derive(Subcommand, Debug)]
enum ExtCommand {
    /// Add an extension dep (delegates to `composer require`, then sync).
    Add { names: Vec<String> },
    /// Remove an extension dep (delegates to `composer remove`, then sync).
    Remove { names: Vec<String> },
    /// List extensions: installed + available, side by side.
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
    Install {
        request: Option<String>,
        #[arg(long)]
        flavor: Option<String>,
    },
    Uninstall {
        request: String,
        #[arg(long)]
        flavor: Option<String>,
    },
    /// List PHP interpreters: installed + available, side by side.
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
    /// Print the absolute path to a satisfying interpreter.
    Find { request: Option<String> },
    /// Pin the project's PHP version (writes to bougie.toml or extra.bougie).
    Pin {
        request: String,
        #[arg(long, conflicts_with = "composer")]
        toml: bool,
        #[arg(long, conflicts_with = "toml")]
        composer: bool,
    },
    /// Refresh installed interpreters to the latest published patch.
    Upgrade { minor: Option<String> },
    /// Print the directory that PHP interpreters are installed in.
    Dir,
}

#[derive(Subcommand, Debug)]
enum CacheCommand {
    /// Wipe `$BOUGIE_CACHE` (index, manifests, partial blob downloads).
    Clean,
    /// GC unreachable store paths from `$BOUGIE_HOME/store/`.
    Prune {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        prune_projects: bool,
    },
    /// Print `$BOUGIE_CACHE`.
    Dir,
    /// Print sizes of cache, store, installs.
    Size,
}

#[derive(Subcommand, Debug)]
enum SelfCommand {
    Update {
        #[arg(long)]
        check: bool,
    },
    Version {
        /// Print the supported `--format` schema names.
        #[arg(long)]
        schemas: bool,
        /// Print the published telemetry schema.
        #[arg(long)]
        telemetry_schema: bool,
        /// Single-value extraction.
        #[arg(long, value_name = "PATH")]
        field: Option<String>,
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
