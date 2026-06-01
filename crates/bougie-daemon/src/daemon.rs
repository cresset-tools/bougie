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
pub mod cgroup;
pub mod ipc;
pub mod logs;
pub mod provisioners;
pub mod sandbox;
mod state;
pub mod store_fetch;
pub mod store_layout;
pub mod supervisor;
pub mod tenants;

/// Saturating conversion of a `Duration` to `u64` of milliseconds.
/// Used for tracing fields and IPC payloads; `Duration::as_millis()`
/// returns `u128`, but truncation is only theoretically reachable past
/// ~584 million years.
#[inline]
fn duration_to_ms_u64(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

use bougie_paths::Paths;
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

    // 1-second supervisor tick. Reaps exited children, transitions
    // Running → Failed, and (Phase 5+) fires deadline-driven restarts.
    {
        let supervisor = Arc::clone(&state.supervisor);
        let mut tick_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        supervisor.lock().await.check_all().await;
                    }
                    _ = tick_rx.changed() => {
                        if *tick_rx.borrow() {
                            return;
                        }
                    }
                }
            }
        });
    }

    // Restore the services that were `up` before this daemon (re)started
    // so a restart — version upgrade, crash, or manual bounce — never
    // orphans them. Off the accept loop and best-effort: the daemon
    // serves immediately and services come back as they spin up.
    {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            restore_services(&state).await;
        });
    }

    accept_loop(listener, Arc::clone(&state), shutdown_rx).await;

    // Drain: stop every running service in reverse start-order so
    // bougied exits with no orphans. Best-effort — surfacing errors
    // here doesn't help anyone since we're already exiting.
    drain(&state).await;

    Ok(ExitCode::SUCCESS)
}

/// Re-spawn the services that were `up` before this daemon (re)started.
///
/// bougied holds its running-set in memory, so a restart — the CLI
/// shutting the old daemon down to autospawn a new one on a version
/// upgrade, a crash, or a manual bounce — would otherwise leave the
/// previously running services orphaned (their babysit children are
/// killed by [`drain`]) with nothing bringing them back. That strands
/// whatever depended on them (e.g. a half-installed Magento mid-recipe).
///
/// The tenant ledger is the persisted record of `up` intent: `bougie up`
/// appends a tenant; `bougie down` removes it and stops the service once
/// the last tenant is gone. So a user-facing service with ≥1 tenant is
/// one the user brought up and never took down — re-spawn it. The work
/// mirrors `dispatch_up`'s idempotent process bring-up (ensure tarball →
/// `pre_start` → `supervisor.start`), minus tenant provisioning (the
/// tenants already exist on disk). Best-effort: failures are logged,
/// never fatal.
async fn restore_services(state: &Arc<DaemonState>) {
    use crate::daemon::{catalog, provisioners, store_fetch, supervisor};

    let wanted = wanted_services(&state.paths).await;
    if wanted.is_empty() {
        return;
    }
    let order = match supervisor::compute_start_order(&wanted) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "restore: cannot order services; skipping");
            return;
        }
    };
    tracing::info!(?wanted, "restore: re-spawning services after daemon (re)start");
    for name in order {
        let Some(entry) = catalog::find(name) else { continue };
        if let Err(e) = store_fetch::ensure_tarball(&state.paths, entry, None).await {
            tracing::warn!(service = name, error = format!("{e:#}"), "restore: tarball fetch");
            continue;
        }
        if let Err(e) = provisioners::pre_start(entry, &state.paths).await {
            tracing::warn!(service = name, error = %e, "restore: pre_start");
            continue;
        }
        match state.supervisor.lock().await.start(name).await {
            Ok(true) => tracing::info!(service = name, "restore: re-spawned"),
            Ok(false) => {}
            Err(e) => tracing::warn!(service = name, error = %e, "restore: start"),
        }
    }
}

/// User-facing catalog services that still have ≥1 provisioned tenant —
/// the set the user has `up` and not `down`. See [`restore_services`].
async fn wanted_services(paths: &Paths) -> Vec<&'static str> {
    use crate::daemon::{catalog, tenants};

    let mut wanted = Vec::new();
    for entry in catalog::CATALOG {
        if !entry.user_facing {
            continue;
        }
        let tenants_path = paths.service_tenants(entry.name);
        let n = tenants::load_all(&tenants_path).await.map_or(0, |v| v.len());
        if n > 0 {
            wanted.push(entry.name);
        }
    }
    wanted
}

async fn drain(state: &Arc<DaemonState>) {
    use crate::daemon::supervisor::ServiceState;
    let running: Vec<&'static str> = {
        let sup = state.supervisor.lock().await;
        sup.snapshot()
            .into_iter()
            .filter(|s| {
                matches!(
                    s.state,
                    ServiceState::Running
                        | ServiceState::HealthChecking
                        | ServiceState::Starting
                )
            })
            // SAFETY: catalog names are 'static; the snapshot copied
            // them into owned Strings, but we can re-resolve to 'static
            // via the catalog itself.
            .filter_map(|s| {
                crate::daemon::catalog::find(&s.name).map(|e| e.name)
            })
            .collect()
    };
    for name in running.iter().rev() {
        let mut sup = state.supervisor.lock().await;
        let _ = sup.stop(name).await;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::catalog;
    use crate::daemon::tenants::{self, Tenant};

    #[tokio::test]
    async fn wanted_services_tracks_tenanted_user_facing_services() {
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let paths = Paths::new(
            home.path().to_path_buf(),
            cache.path().to_path_buf(),
        );

        // Fresh state: nothing provisioned → nothing to restore.
        assert!(wanted_services(&paths).await.is_empty());

        // Provision a tenant for the first user-facing service (as
        // `bougie up` would). It should now be in the restore set.
        let svc = catalog::CATALOG
            .iter()
            .find(|e| e.user_facing)
            .expect("a user-facing service in the catalog")
            .name;
        let tenants_path = paths.service_tenants(svc);
        std::fs::create_dir_all(tenants_path.parent().unwrap()).unwrap();
        tenants::append(&tenants_path, &Tenant::new("acme", "/tmp/acme"))
            .await
            .unwrap();

        let wanted = wanted_services(&paths).await;
        assert!(wanted.contains(&svc), "expected {svc} in {wanted:?}");
    }
}
