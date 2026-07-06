//! `bougied` — the per-user service supervisor daemon.
//!
//! Same binary as `bougie`, dispatched via `argv[0] == "bougied"`
//! through `src/shim.rs`. The CLI auto-spawns the daemon on the first
//! `bougie service …` invocation; subsequent commands reuse the
//! running daemon over the Unix socket at
//! `$BOUGIE_HOME/state/bougied.sock` (mode 0600).
//!
//! Phase 1 ships the listener, signal handling, singleton enforcement
//! via flock on `bougied.pid`, and the daemon-level IPC methods
//! (`status`, `daemon.version`, `daemon.shutdown`). Service
//! supervision lands in Phase 3.

pub mod catalog;
pub mod cgroup;
pub mod credentials;
pub mod health;
pub mod ipc;
pub mod logs;
pub mod provisioners;
pub mod sandbox;
mod state;
pub mod store_fetch;
pub mod store_layout;
pub mod supervisor;
pub mod tenant_env;
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
    // Detach into our own session before doing anything else.
    //
    // The CLI auto-spawns us as a plain child sharing its session and
    // foreground process group (see
    // commands/services/client.rs::spawn_daemon). Without `setsid`, a
    // Ctrl-C in the terminal that first ran `bougie server`/`bougie up`
    // delivers SIGINT to the *whole* foreground group — bougied, every
    // bougie-babysit, and the services they own — so detaching from a
    // log stream would tear the stack down. (Linux's PR_SET_PDEATHSIG
    // masks this in some flows; macOS has no equivalent, so it bites
    // there reliably.) `setsid` makes us a session + group leader with
    // no controlling terminal, so terminal-generated signals never
    // reach us. Best-effort: it returns EPERM when we're already a
    // group leader (e.g. `bougied` run straight from an interactive
    // shell, where job control already put us in our own group), which
    // is exactly the case where we're already detached enough — so a
    // failure here is benign and we carry on.
    let _ = rustix::process::setsid();

    std::fs::create_dir_all(paths.state())
        .wrap_err_with(|| format!("creating {}", paths.state().display()))?;

    // Anchor our cwd to the state root before we do anything else.
    //
    // bougied is long-lived and auto-spawned by the first `bougie
    // services …` call (see commands/services/client.rs::spawn_daemon),
    // so it inherits — and would otherwise keep forever — the cwd of
    // whatever directory that invocation ran from. When the operator
    // runs bougie from a throwaway project/temp dir and then deletes it,
    // the daemon is left holding an unlinked cwd. Every child we spawn
    // without an explicit current_dir (the rabbitmqctl / mariadb-client
    // provisioner probes especially) then inherits that dead cwd, and
    // any of them that does a `getcwd()` at startup — notably Erlang's
    // BEAM, which `rabbitmqctl` and `rabbitmq-server` both are — aborts
    // with `invalid_current_directory` ("getcwd: cannot access parent
    // directories: No such file or directory"). The supervisor already
    // pins individual services via `render_exec_cwd`, but the out-of-
    // band ctl probes don't go through it; fixing the daemon's own cwd
    // at the source covers them all. The state root is created just
    // above, is owned by us, and outlives any project dir, so it's the
    // stable, writable anchor — stray crash dumps (erl_crash.dump) land
    // somewhere sane instead of "/" or a vanished directory.
    std::env::set_current_dir(paths.state())
        .wrap_err_with(|| format!("anchoring daemon cwd to {}", paths.state().display()))?;

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
    // Stamp our PID so external diagnostics (`bougie service daemon
    // status`, ps grep, etc.) can find us.
    pid_file.set_len(0).ok();
    writeln!(&pid_file, "{}", std::process::id()).ok();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("bougied")
        .build()
        .wrap_err("building tokio runtime")?;

    let exit = rt.block_on(serve(paths.clone()))?;

    // Best-effort socket cleanup; `serve` also removes a stale socket on
    // the next start, so either way is fine.
    let _ = std::fs::remove_file(paths.bougied_sock());
    // Deliberately do NOT unlink the pidfile. The flock singleton is keyed
    // on the pidfile's *inode*: unlinking it here opens a race where a
    // contender that already `open`ed the old inode flocks it (succeeding
    // once we drop our fd) while a third `open` on the same path creates a
    // *fresh* inode and flocks that independently — leaving two daemons
    // live at once. Leaving the pidfile in place means every contender
    // opens and flocks the same inode; dropping our fd releases the lock
    // for the next one, which truncates and rewrites the stale PID on
    // start (see the open-then-flock above).
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

    // Reap leftover service cgroups from a previous bougied that died
    // without cleaning up. The flock singleton guarantees we're the only
    // live instance *for this home*, and the reap is scoped to this
    // home's namespaced cgroup dir — concurrent daemons with other
    // `BOUGIE_HOME`s in the same session keep theirs (#456). Done before
    // `restore_services` so re-spawns start from a clean slate. No-op
    // under the process-group backend.
    state.supervisor.lock().await.reap_stale_leaves().await;

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
        let paths = state.paths.clone();
        let mut tick_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        // Reap exited children under the lock, then drive
                        // each due auto-restart off the lock: a service's
                        // health probe runs up to 90s and must not stall
                        // reaping or `status`. Detached (not awaited) so
                        // the ticker keeps reaping every second regardless
                        // of probe latency.
                        let due = supervisor.lock().await.check_all().await;
                        for name in due {
                            let sup = Arc::clone(&supervisor);
                            tokio::spawn(async move {
                                if let Err(e) =
                                    crate::daemon::supervisor::start_service(&sup, name).await
                                {
                                    tracing::warn!(
                                        service = name,
                                        error = %e,
                                        "auto-restart failed; will retry on a later backoff tick"
                                    );
                                    sup.lock().await.note_restart_failure(name);
                                }
                            });
                        }

                        // Continuous health: re-probe live services whose
                        // probe is due, off the lock (same discipline as
                        // start — a probe can take seconds). On a sustained
                        // failure the service is torn down + rescheduled,
                        // so a wedged-but-alive process no longer hides as
                        // "Running". See `health::probe` and the
                        // `Supervisor::health_*` methods.
                        let health_due = supervisor.lock().await.health_due();
                        for name in health_due {
                            let sup = Arc::clone(&supervisor);
                            let paths = paths.clone();
                            tokio::spawn(async move {
                                let ok = crate::daemon::health::probe(name, &paths)
                                    .await
                                    .is_ok();
                                let outcome = sup.lock().await.record_health(name, ok);
                                if outcome == crate::daemon::supervisor::HealthOutcome::Breach {
                                    sup.lock().await.fail_unhealthy(name).await;
                                }
                            });
                        }
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

    // Drop our (now-empty) namespaced service-cgroup dir so short-lived
    // homes don't pile empty namespaces into the session scope until
    // logout. rmdir on a non-empty cgroup fails, which is the right
    // outcome if any leaf survived drain — the next startup's
    // `reap_stale_leaves` deals with it instead.
    state.supervisor.lock().await.remove_svc_root();

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
        match supervisor::start_service(&state.supervisor, name).await {
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
                        | ServiceState::Unhealthy
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
