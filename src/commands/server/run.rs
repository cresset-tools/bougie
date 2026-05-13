//! Foreground entry point. Builds the tokio runtime, parses
//! server.toml, applies CLI overrides, indexes hosts, binds the
//! listener, and serves until SIGINT/SIGTERM. Spec: SERVER.md §2, §5,
//! §8, §9.
//!
//! Drain ceiling on shutdown is 5s — long enough to finish in-flight
//! responses, short enough that `^C; bougie server` round-trips don't
//! feel sluggish.

use crate::cli::OutputFormat;
use crate::paths::Paths;
use eyre::{Result, WrapErr};
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use super::config;
use super::log::{self, LogFormat};
use super::paths::ServerPaths;
use super::pool::PoolManager;
use super::router::{self, AppState};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub fn run(
    _format: OutputFormat,
    _field: Option<&str>,
    config_override: Option<&Path>,
    listen_override: Option<&str>,
    log_format_override: Option<&str>,
) -> Result<ExitCode> {
    let config_path = config::resolve_path(config_override)?;
    let cfg = config::load(&config_path)?;

    let listen_str = listen_override.unwrap_or(&cfg.server.listen);
    let listen: SocketAddr = listen_str
        .parse()
        .wrap_err_with(|| format!("invalid listen address: {listen_str:?}"))?;

    let log_fmt_str = log_format_override.unwrap_or(&cfg.server.log_format);
    let log_fmt = LogFormat::parse(log_fmt_str).map_err(eyre::Report::msg)?;
    log::init(log_fmt);

    let bougie_paths = Paths::from_env()?;
    let server_paths = ServerPaths::from_env()?;
    let pools = Arc::new(PoolManager::new(
        bougie_paths,
        server_paths,
        cfg.server.debug_only_extensions.clone(),
    ));

    // Hostname-collision check happens here so config errors surface
    // before we bind a port (§spec implementation note).
    let state = Arc::new(AppState::build(&cfg, Arc::clone(&pools), listen.port())?);

    if state.hosts.is_empty() {
        eprintln!(
            "bougie: no hosts configured in {} — run `bougie server add` first.",
            config_path.display()
        );
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("bougie-server")
        .build()
        .wrap_err("building tokio runtime")?;

    let exit = rt.block_on(async move { serve(listen, state).await })?;
    Ok(exit)
}

async fn serve(listen: SocketAddr, state: Arc<AppState>) -> Result<ExitCode> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .wrap_err_with(|| format!("binding {listen}"))?;
    let bound = listener
        .local_addr()
        .map_or_else(|_| listen.to_string(), |a| a.to_string());
    eprintln!("bougie server listening on http://{bound} ({} hosts)", state.hosts.len());

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let signal_rx = shutdown_rx.clone();
    let pools_for_shutdown = Arc::clone(&state.pools);
    let app = router::build(state).into_make_service_with_connect_info::<SocketAddr>();
    let serve_fut = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let mut rx = signal_rx;
            let _ = rx.wait_for(|v| *v).await;
        })
        .into_future();
    tokio::pin!(serve_fut);

    let signal_task = tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    // Phase A: run until shutdown is signalled or the server stops on
    // its own (e.g. bind error mid-flight).
    let mut shutdown_started = shutdown_rx;
    tokio::select! {
        res = &mut serve_fut => {
            signal_task.abort();
            return res.map(|()| ExitCode::SUCCESS).wrap_err("serving HTTP");
        }
        _ = shutdown_started.wait_for(|v| *v) => {}
    }

    // Phase B: drain. Cap the wait — a hung backend shouldn't strand
    // `^C`. Phase 2's FastCGI dispatch is the real source of slow
    // drains; the cap is sized for that future case.
    tokio::select! {
        res = &mut serve_fut => {
            res.wrap_err("serving HTTP")?;
        }
        () = tokio::time::sleep(SHUTDOWN_GRACE) => {
            eprintln!("bougie server: shutdown grace ({SHUTDOWN_GRACE:?}) elapsed, exiting hard");
        }
    }

    // Reap every php-fpm master we spawned. `kill_on_drop(true)` on
    // each `Child` is the belt; `terminate()` is the braces.
    pools_for_shutdown.shutdown().await;

    Ok(ExitCode::SUCCESS)
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = sigint.recv() => eprintln!("\nbougie server: SIGINT received, shutting down"),
        _ = sigterm.recv() => eprintln!("bougie server: SIGTERM received, shutting down"),
    }
}
