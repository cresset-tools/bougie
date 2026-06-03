use bougie::{exit_code_for, shim, Cli};
use clap::Parser;
use std::process::ExitCode;

// SIGQUIT (Ctrl-\) activity dump. Unix-only: it hangs off POSIX signals
// and the `tracing` span machinery. On Windows `init_tracing` installs
// just the fmt subscriber.
#[cfg(unix)]
mod debug_dump;

fn main() -> ExitCode {
    init_tracing();
    let argv0 = std::env::args_os().next().unwrap_or_default();
    if let Some(role) = shim::role_from_argv0(&argv0) {
        return match shim::exec(role) {
            Ok(code) => code,
            Err(err) => {
                report_error(&err);
                ExitCode::from(exit_code_for(&err))
            }
        };
    }

    // Register the bougie-recipe → bougied IPC bridge. Recipe and the
    // bridge both live behind cfg(unix); on Windows the recipe crate is
    // an empty lib and there's nothing to register.
    #[cfg(unix)]
    bougie_recipe::set_service_env_provider(bougie::commands::services::recipe_env_for_project);

    let cli = Cli::parse();
    match bougie::run(cli) {
        Ok(code) => code,
        Err(err) => {
            report_error(&err);
            ExitCode::from(exit_code_for(&err))
        }
    }
}

/// Install a `tracing-subscriber` configured from the environment.
/// Reads `BOUGIE_LOG` (preferred — namespaced so it can't collide with
/// a dependency's `RUST_LOG` use), then falls back to `RUST_LOG`. When
/// neither is set the subscriber is still installed but its filter
/// rejects every record, so call sites stay zero-overhead.
///
/// Output goes to stderr with timestamps and target names so the user
/// can correlate spans across crates:
///   `BOUGIE_LOG=bougie_composer_resolver=debug bougie composer update`
/// also shows per-package fetch timings via `tracing::debug_span!`.
fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    use tracing_subscriber::{EnvFilter, Layer as _};

    let filter = std::env::var("BOUGIE_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|s| EnvFilter::try_new(s).ok())
        .unwrap_or_else(|| EnvFilter::new("off"));

    // The fmt layer keeps its env filter (inert unless BOUGIE_LOG/RUST_LOG
    // is set). On Unix we stack the activity layer alongside it — it runs
    // unfiltered so a Ctrl-\ dump works even with logging off — then arm
    // the SIGQUIT handler that prints the dump.
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_filter(filter);

    #[cfg(unix)]
    {
        let _ = tracing_subscriber::registry()
            .with(fmt_layer)
            .with(debug_dump::layer())
            .try_init();
        debug_dump::install_signal_handler();
    }
    #[cfg(not(unix))]
    {
        let _ = tracing_subscriber::registry().with(fmt_layer).try_init();
    }
}

/// Render an error to stderr in uv's `error: <message>` style.
///
/// Multi-line error messages (from `BougieError` variants that include
/// hint blocks) keep their structure; subsequent lines are indented to
/// align under the first line. Cause chain entries follow on
/// `  caused by:` lines, mirroring uv.
fn report_error(err: &eyre::Report) {
    let mut chain = err.chain();
    let head = match chain.next() {
        Some(c) => c.to_string(),
        None => return,
    };
    let mut lines = head.lines();
    if let Some(first) = lines.next() {
        eprintln!("error: {first}");
    }
    for rest in lines {
        eprintln!("       {rest}");
    }
    for cause in chain {
        let s = cause.to_string();
        let mut cl = s.lines();
        if let Some(first) = cl.next() {
            eprintln!("  caused by: {first}");
        }
        for rest in cl {
            eprintln!("             {rest}");
        }
    }
}
