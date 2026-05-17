//! Foreground entry point. Builds the tokio runtime, parses
//! server.toml, applies CLI overrides, indexes hosts, binds the
//! listener, and serves until SIGINT/SIGTERM. Spec: SERVER.md §2, §5,
//! §8, §9.
//!
//! Drain ceiling on shutdown is 5s — long enough to finish in-flight
//! responses, short enough that `^C; bougie server` round-trips don't
//! feel sluggish.

use bougie_cli::OutputFormat;
use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use super::config;
use super::control;
use super::log::{self, LogFormat};
use super::paths::ServerPaths;
use super::pool::{self, PoolManager};
use super::router::{self, AppState};
use super::watcher;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub fn run(
    _format: OutputFormat,
    config_path: &Path,
    listen_override: Option<&str>,
    log_format_override: Option<&str>,
) -> Result<ExitCode> {
    let config_path = config_path.to_path_buf();
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
    let idle_pool_timeout = cfg.server.idle_pool_timeout_duration()?;
    let max_concurrent_pools = usize::try_from(cfg.server.max_concurrent_pools)
        .unwrap_or(16);
    let pools = Arc::new(PoolManager::new(
        bougie_paths,
        server_paths,
        idle_pool_timeout,
        max_concurrent_pools,
    ));

    // Hostname-collision check happens here so config errors surface
    // before we bind a port (§spec implementation note).
    let state = Arc::new(AppState::build(
        &cfg,
        config_path.clone(),
        Arc::clone(&pools),
        listen.port(),
    )?);

    if state.hosts.read().expect("hosts lock poisoned").is_empty() {
        eprintln!(
            "bougie: no hosts configured in {} — add `[[host]]` blocks to that file, \
             or run via `bougie up server` so bougied manages it for you.",
            config_path.display()
        );
    }

    // Walk every configured [[host]] and surface filesystem
    // discrepancies (missing project dir, missing web root, no index
    // file). The server still starts — these are heads-up warnings so
    // users debug a 404 with information instead of silence.
    for host in &cfg.hosts {
        super::helpers::warn_host(host);
    }

    // Unique project list dedup'd here so two hostnames pointing at
    // the same project share one filesystem watch.
    let projects: Vec<std::path::PathBuf> = {
        let mut seen = std::collections::HashSet::new();
        cfg.hosts
            .iter()
            .map(|h| h.project.clone())
            .filter(|p| seen.insert(p.clone()))
            .collect()
    };

    // Prune stale per-project runtime dirs from earlier server runs
    // that exited abnormally (SIGKILL, panic, machine reboot leaving
    // bind-mounted XDG_RUNTIME_DIR). The graceful shutdown path
    // already removes our own dirs; this catches everything else.
    let server_paths_for_prune = ServerPaths::from_env()?;
    for (path, err) in server_paths_for_prune.prune_project_dirs(&projects) {
        eprintln!(
            "bougie server: failed to prune stale runtime dir {}: {err}",
            path.display()
        );
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("bougie-server")
        .build()
        .wrap_err("building tokio runtime")?;

    let control_socket = ServerPaths::from_env()?.control_socket();
    let exit = rt
        .block_on(async move { serve(listen, state, projects, control_socket).await })?;
    Ok(exit)
}

async fn serve(
    listen: SocketAddr,
    state: Arc<AppState>,
    projects: Vec<std::path::PathBuf>,
    control_socket: std::path::PathBuf,
) -> Result<ExitCode> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .wrap_err_with(|| format!("binding {listen}"))?;
    if let Ok(addr) = listener.local_addr() {
        state.set_listen_port(addr.port());
    }
    let bound = listener
        .local_addr()
        .map_or_else(|_| listen.to_string(), |a| a.to_string());
    let host_count = state.hosts.read().expect("hosts lock poisoned").len();
    eprintln!("bougie server listening on http://{bound} ({host_count} hosts)");

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let signal_rx = shutdown_rx.clone();
    let pools_for_shutdown = Arc::clone(&state.pools);
    let reaper_task = pool::start_idle_reaper(Arc::clone(&state.pools));
    let _watcher_handle = match watcher::start(&projects, &state.pools) {
        Ok(h) => Some(h),
        Err(e) => {
            // A failure here is non-fatal — pools still serve, reload
            // just won't fire. Surface a warning and continue.
            eprintln!("bougie server: filesystem watcher unavailable: {e:#}");
            None
        }
    };
    let _control_handle = match control::start(Arc::clone(&state), control_socket.clone()) {
        Ok(h) => {
            eprintln!("bougie server: control socket at {}", control_socket.display());
            Some(h)
        }
        Err(e) => {
            // Non-fatal: `bougie server list` will fall back to
            // config-only output.
            eprintln!("bougie server: control socket unavailable: {e:#}");
            None
        }
    };
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
    let drain_result = tokio::select! {
        res = &mut serve_fut => res.wrap_err("serving HTTP"),
        () = tokio::time::sleep(SHUTDOWN_GRACE) => {
            eprintln!("bougie server: shutdown grace ({SHUTDOWN_GRACE:?}) elapsed, exiting hard");
            Ok(())
        }
    };

    // Reap every php-fpm master we spawned. `kill_on_drop(true)` on
    // each `Child` is the belt; `terminate()` is the braces.
    reaper_task.abort();
    pools_for_shutdown.shutdown().await;

    // Pools are dead — wipe every per-project runtime dir we
    // created. Anything under `$XDG_RUNTIME_DIR/bougie/server/`
    // matching the project-hash naming convention is fair game; the
    // control socket itself is a file, not a subdir, so it survives.
    // This runs even if phase B reported an error so an aborted serve
    // doesn't leak stale state — same idea as a defer/Drop guard.
    if let Ok(sp) = ServerPaths::from_env() {
        for (path, err) in sp.prune_project_dirs(&[]) {
            eprintln!(
                "bougie server: failed to clean up runtime dir {}: {err}",
                path.display()
            );
        }
    }

    drain_result?;
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
