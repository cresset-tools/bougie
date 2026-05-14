//! `bougied` — the per-user service supervisor daemon.
//!
//! Same binary as `bougie`, dispatched via `argv[0] == "bougied"`
//! through `src/shim.rs`. The CLI auto-spawns the daemon on the first
//! `bougie services …` invocation; subsequent commands reuse the
//! running daemon over the Unix socket at
//! `$BOUGIE_HOME/state/bougied.sock` (mode 0600).
//!
//! Phase 1 ships the listener, signal handling, singleton enforcement
//! via flock on `bougied.pid`, and the daemon-level IPC methods
//! (`status`, `daemon.version`, `daemon.shutdown`). Service
//! supervision lands in Phase 3.

pub mod catalog;
pub mod ipc;
mod state;

use crate::Paths;
use eyre::{Result, WrapErr};
use rustix::fs::{flock, FlockOperation};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::watch;

use state::DaemonState;

/// Entry point for the `bougied` argv[0] role. Called from `shim::exec`.
pub fn run(paths: Paths) -> Result<ExitCode> {
    std::fs::create_dir_all(paths.state())
        .wrap_err_with(|| format!("creating {}", paths.state().display()))?;

    // Singleton: an exclusive flock on bougied.pid blocks a second
    // daemon from starting. The fd is held for the full daemon
    // lifetime so the kernel keeps the lock; on exit (clean or
    // crash) the kernel releases it for the next contender.
    let pid_path = paths.bougied_pid();
    let pid_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&pid_path)
        .wrap_err_with(|| format!("opening {}", pid_path.display()))?;
    flock(&pid_file, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
        eyre::eyre!(
            "another bougied is already running (could not flock {}: {})",
            pid_path.display(),
            e
        )
    })?;
    // Stamp our PID so external diagnostics (`bougie services daemon
    // status`, ps grep, etc.) can find us.
    pid_file.set_len(0).ok();
    writeln!(&pid_file, "{}", std::process::id()).ok();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("bougied")
        .build()
        .wrap_err("building tokio runtime")?;

    let exit = rt.block_on(serve(paths.clone()))?;

    // Best-effort cleanup. Either path is fine on next start —
    // the open-then-flock pattern survives stale pid/sock files.
    let _ = std::fs::remove_file(paths.bougied_sock());
    let _ = std::fs::remove_file(&pid_path);
    drop(pid_file); // release flock

    Ok(exit)
}

async fn serve(paths: Paths) -> Result<ExitCode> {
    let sock_path = paths.bougied_sock();
    // Remove any stale socket from a previous run that exited
    // abnormally; the kernel doesn't auto-clean unix sockets the way
    // it does abstract ones.
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)
        .wrap_err_with(|| format!("binding {}", sock_path.display()))?;
    // Mode 0600: only the owning user may connect. Bougie is
    // per-user, so this is the entire authorization story.
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(&sock_path, perms)
        .wrap_err_with(|| format!("chmod 0600 on {}", sock_path.display()))?;
    tracing::info!(socket = %sock_path.display(), "bougied: listening");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state = Arc::new(DaemonState::new(paths, shutdown_tx.clone()));

    // SIGTERM / SIGINT flip the shutdown flag. The accept loop
    // observes the flag and exits.
    {
        let shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            let _ = shutdown_tx.send(true);
        });
    }

    accept_loop(listener, state, shutdown_rx).await;

    Ok(ExitCode::SUCCESS)
}

async fn accept_loop(
    listener: UnixListener,
    state: Arc<DaemonState>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            // shutdown wins — drain immediately
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("bougied: shutdown signaled, exiting accept loop");
                    return;
                }
            }
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _addr)) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            ipc::handle_connection(stream, state).await;
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "bougied: accept failed");
                        // Brief back-off: a persistent EAGAIN would
                        // otherwise pin a CPU here.
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = sigint.recv() => tracing::info!("bougied: SIGINT received"),
        _ = sigterm.recv() => tracing::info!("bougied: SIGTERM received"),
    }
}
