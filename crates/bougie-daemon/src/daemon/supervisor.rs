//! Process supervisor: state machine + spawn-with-sandbox + health
//! probes + two-phase stop + topological start order.
//!
//! Phase 3 ships only what redis needs to come online end-to-end.
//! Restart policy, log rotation, and the broader catalog provisioner
//! dispatch land in subsequent phases (5–10).

use super::catalog::{self, Binding, CatalogEntry};
use super::logs::LogWriter;
use super::sandbox;
use super::store_layout;
use bougie_paths::Paths;
use eyre::{eyre, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::process::Child;
use tokio::sync::Mutex;

/// Default per-service health-probe budget. Redis comes up in
/// <100ms; mariadb cold-starts in ~3-5s; opensearch needs
/// significantly more because the JVM has to JIT-compile + bootstrap
/// the cluster state. `health_timeout_for` overrides this per
/// service for the slow ones.
const HEALTH_TIMEOUT_DEFAULT: Duration = Duration::from_mins(1);
const HEALTH_POLL: Duration = Duration::from_millis(250);

/// Auto-restart on failure. Exponential backoff, capped, with a
/// "reset on sustained run" rule so a service that briefly health-
/// passes then dies 2s later doesn't masquerade as a first failure
/// each cycle. See SERVICES.md §5.1.
const BASE_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_mins(5);
/// If the previous successful Running window exceeded this, treat
/// the next failure as "first failure" again (`failure_count = 1`).
const FAILURE_RESET_THRESHOLD: Duration = Duration::from_mins(1);
/// After this many consecutive failures, stop respawning. The
/// service is conclusively broken; the operator can `bougie
/// services up <name>` to retry manually.
const MAX_RESTART_ATTEMPTS: u32 = 10;

/// Per-service health-probe deadline. JVM-based services (opensearch
/// today, rabbitmq via erlang/JIT later) need a longer window because
/// JIT compilation + cluster bootstrap dominate cold-start time.
fn health_timeout_for(name: &str) -> Duration {
    match name {
        "opensearch" => Duration::from_secs(90),
        // Erlang VM boot + mnesia bootstrap + plugin discovery puts
        // rabbitmq's cold-start time well past the 60s default on
        // slower CI runners.
        "rabbitmq" => Duration::from_secs(90),
        _ => HEALTH_TIMEOUT_DEFAULT,
    }
}

/// Default grace window before escalating SIGTERM → SIGKILL. Matches
/// SERVICES.md §5.3.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Per-babysit grace window. Strictly less than [`STOP_GRACE`] so the
/// supervisor's outer wait outlives the inner SIGTERM→SIGKILL escalation
/// the babysit performs.
const BABYSIT_GRACE_SECS: u64 = 7;

/// Max time the supervisor waits for the babysit to report the
/// service's `pgid=` line over the control socket. If the babysit
/// failed before writing it, this collapses to "spawn failed."
const BABYSIT_READY_TIMEOUT: Duration = Duration::from_secs(1);

/// Fd number on which the babysit expects its end of the control
/// socketpair after `dup2` in `pre_exec`.
const BABYSIT_CONTROL_FD: i32 = 3;

/// Lifecycle states. The same shape as project-supervisor's
/// `ManagedService` but with `HealthChecking` broken out — the daemon
/// shouldn't claim a service is `Running` until something can actually
/// connect to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Stopped,
    Starting,
    HealthChecking,
    Running,
    Stopping,
    Failed,
}

/// One supervised service. The `child` is `None` whenever the state
/// is `Stopped` or `Failed`.
///
/// `child` is the *babysit* process (see the `bougie-babysit` crate); the
/// service binary itself is `child`'s grandchild via `fork+exec` into
/// a fresh process group. `service_pgid` is the babysit-reported
/// pgid we use for group-wide signals (`kill(-pgid, ...)`).
/// `control_sock` is held open for the full supervised lifetime —
/// dropping it before stop would EOF the babysit and trigger an
/// unintended self-stop.
#[derive(Debug)]
pub struct ManagedService {
    pub name: &'static str,
    pub state: ServiceState,
    pub child: Option<Child>,
    /// PID of the babysit shim.
    pub pid: Option<u32>,
    /// Process group id of the service the babysit owns.
    pub service_pgid: Option<i32>,
    /// `/proc/<pgid>/stat` start-time of the group leader, captured when
    /// `service_pgid` was learned. Used as a PID-reuse guard before
    /// group-wide signals: if the kernel recycled the pgid onto an
    /// unrelated process group, the start-time won't match. Linux-only
    /// (None elsewhere — best-effort).
    pub service_pgid_starttime: Option<u64>,
    /// Parent end of the bougied↔babysit socketpair. Holding this
    /// open keeps the babysit alive; dropping it tells the babysit
    /// (via socket EOF) to clean up the group.
    pub control_sock: Option<tokio::net::UnixStream>,
    pub started_at: Option<Instant>,
    /// Consecutive crash count for backoff purposes. Reset to 0 when
    /// the next start succeeds AND the previous Running window was
    /// at least `FAILURE_RESET_THRESHOLD` (handled at Failed time
    /// using `started_at`).
    pub failure_count: u32,
    /// When the most recent transition into Failed happened.
    /// Diagnostic only; backoff math uses `restart_at` directly.
    pub last_failure_at: Option<Instant>,
    /// Deadline for the next auto-respawn. `Some` only while
    /// state == Failed and `failure_count <= MAX_RESTART_ATTEMPTS`.
    /// The 1s ticker checks this and calls `start` when due.
    pub restart_at: Option<Instant>,
}

impl ManagedService {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            state: ServiceState::Stopped,
            child: None,
            pid: None,
            service_pgid: None,
            service_pgid_starttime: None,
            control_sock: None,
            started_at: None,
            failure_count: 0,
            last_failure_at: None,
            restart_at: None,
        }
    }
}

/// Snapshot returned by the IPC `status` method. Stable shape; the
/// daemon owns the live state via `Supervisor`.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub state: ServiceState,
    pub pid: Option<u32>,
    pub uptime_ms: Option<u64>,
    pub binding: Binding,
    /// Consecutive failure count. `0` when the service hasn't
    /// crashed recently. Inspected by `bougie services daemon
    /// status` so users can see backoff state.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub failure_count: u32,
    /// Milliseconds until the supervisor will respawn this service.
    /// `Some` only while state == Failed and a respawn is scheduled
    /// (i.e. `failure_count` <= `MAX_RESTART_ATTEMPTS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_restart_ms: Option<u64>,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

#[derive(Debug)]
pub struct Supervisor {
    services: HashMap<&'static str, ManagedService>,
    paths: Paths,
    /// How this host can kill a service's whole subtree. Detected once
    /// at construction. Phase 1: recorded + logged, not yet consumed —
    /// teardown still uses the babysit's process-group path. Later
    /// phases route through this to use per-service cgroups when
    /// available. See `SUPERVISION_PLAN.md`.
    backend: super::cgroup::SupervisionBackend,
}

impl Supervisor {
    pub fn new(paths: Paths) -> Self {
        let mut services = HashMap::new();
        for entry in catalog::CATALOG {
            services.insert(entry.name, ManagedService::new(entry.name));
        }
        let backend = super::cgroup::detect();
        tracing::info!(
            backend = backend.label(),
            base = backend.base().map(|p| p.display().to_string()),
            "bougied: supervision backend"
        );
        Self { services, paths, backend }
    }

    /// The detected supervision backend (process-group fallback or a
    /// delegated cgroup-v2 subtree).
    pub fn backend(&self) -> &super::cgroup::SupervisionBackend {
        &self.backend
    }

    /// Kill + remove leftover service leaf cgroups from a previous
    /// bougied that died without cleaning up. Called once at startup —
    /// the flock singleton guarantees no other live instance, so any
    /// leaves present are orphans safe to reap. No-op under the
    /// process-group backend. Runs on the blocking pool.
    pub async fn reap_stale_leaves(&self) {
        let Some(base) = self.backend.base().map(std::path::Path::to_path_buf) else {
            return;
        };
        let kill_supported = self.backend.kill_supported();
        let reaped = tokio::task::spawn_blocking(move || {
            super::cgroup::reap_stale_leaves(&base, kill_supported)
        })
        .await
        .unwrap_or_default();
        if !reaped.is_empty() {
            tracing::warn!(?reaped, "reaped leftover service cgroups from a dead bougied");
        }
    }

    /// Snapshot every service for the `status` IPC method.
    pub fn snapshot(&self) -> Vec<ServiceStatus> {
        let now = Instant::now();
        let mut out: Vec<_> = self
            .services
            .values()
            .filter_map(|svc| {
                let entry = catalog::find(svc.name)?;
                let next_restart_ms = svc.restart_at.map(|deadline| {
                    super::duration_to_ms_u64(deadline.saturating_duration_since(now))
                });
                Some(ServiceStatus {
                    name: svc.name.to_string(),
                    state: svc.state,
                    pid: svc.pid,
                    uptime_ms: svc
                        .started_at
                        .map(|t| super::duration_to_ms_u64(t.elapsed())),
                    binding: entry.binding,
                    failure_count: svc.failure_count,
                    next_restart_ms,
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Resolve the on-disk path of a service's main binary. Thin
    /// wrapper over `store_layout::binary` so the supervisor and the
    /// per-service provisioners agree on where each service lives.
    fn binary_path(&self, entry: &CatalogEntry) -> Result<std::path::PathBuf> {
        store_layout::binary(&self.paths, entry)
    }

    /// Spawn a service if it isn't already running. Walks
    /// Stopped → Starting → `HealthChecking` → Running. Returns `true`
    /// if this call brought the service up; `false` if it was already
    /// running.
    ///
    /// # Panics
    ///
    /// Panics on a BUG: the inner `services.get_mut(name).unwrap()`
    /// calls assume `name` is in the map, which `Supervisor::new`
    /// populates from the catalog. A panic here means the catalog
    /// shifted under us mid-call.
    pub async fn start(&mut self, name: &str) -> Result<bool> {
        let entry = catalog::find(name)
            .ok_or_else(|| eyre!("unknown service `{name}`"))?;
        // Idempotence check via an immutable borrow that ends here.
        {
            let svc = self
                .services
                .get(entry.name)
                .ok_or_else(|| eyre!("BUG: service `{}` missing from supervisor map", entry.name))?;
            if matches!(
                svc.state,
                ServiceState::Running | ServiceState::HealthChecking | ServiceState::Starting
            ) {
                return Ok(false);
            }
        }
        // Clear any pending auto-restart deadline — we're starting
        // now, either from operator action or from the ticker firing
        // a due restart. `failure_count` carries over until either a
        // successful sustained run resets it (handled in check_all)
        // or another failure increments it.
        if let Some(svc) = self.services.get_mut(entry.name) {
            svc.restart_at = None;
        }

        // All immutable-self work first so we can later take a single
        // mutable borrow without conflicting with these reads.
        //
        // Resolve the babysit binary (this bougie executable, re-exec'd
        // with an argv[0] override) up front: the sandbox must grant
        // read+exec on it, since it usually lives under $HOME
        // (`~/.local/...`) which `ProtectHome::Yes` otherwise denies —
        // without the carve-in the child can't even `execve` babysit.
        let bougie_bin = std::env::current_exe()
            .wrap_err("locating current_exe for bougie-babysit")?;
        let policy = sandbox::build_policy(entry, &self.paths, &bougie_bin)
            .wrap_err_with(|| format!("compiling sandbox policy for {}", entry.name))?;
        let binary = self.binary_path(entry)?;
        let args = render_exec_args(entry, &self.paths);
        let log_path = self
            .paths
            .service_log(entry.name)
            .join(format!("{}.log", entry.name));
        // Open the LogWriter eagerly — confirms the parent dir is
        // writable before we fork a child. Wrap in Arc<Mutex<…>> so
        // the two stdio forwarder tasks (stdout, stderr) can share
        // it; rotation under live writes is then serialised by the
        // mutex.
        let log_writer = LogWriter::open(log_path)
            .wrap_err_with(|| format!("opening log writer for {}", entry.name))?;
        let log_writer = Arc::new(Mutex::new(log_writer));

        let env = render_exec_env(entry, &self.paths);
        let cwd = render_exec_cwd(entry, &self.paths);

        // bougied does not exec the service directly: it spawns a
        // `bougie-babysit` shim (same binary, argv[0] override) that
        // re-execs the service into its own process group. The
        // socketpair gives the babysit a death-detection signal —
        // when bougied exits, the parent end EOFs and the babysit
        // tears its group down.
        let (parent_sock, child_sock) = std::os::unix::net::UnixStream::pair()
            .wrap_err("creating babysit socketpair")?;
        let child_sock_fd = {
            use std::os::fd::AsRawFd;
            child_sock.as_raw_fd()
        };

        let mut babysit_argv: Vec<std::ffi::OsString> = Vec::with_capacity(args.len() + 9);
        babysit_argv.push("--service-name".into());
        babysit_argv.push(entry.name.into());
        babysit_argv.push("--grace-secs".into());
        babysit_argv.push(BABYSIT_GRACE_SECS.to_string().into());
        babysit_argv.push("--control-fd".into());
        babysit_argv.push(BABYSIT_CONTROL_FD.to_string().into());
        // Co-located helper daemon (today: rabbitmq's `epmd`), started
        // in-group ahead of the service so teardown's `killpg`/cgroup
        // reaps it on every platform — no daemonized escapee.
        if let Some((sidecar, ready_port)) = sidecar_for(entry, &self.paths) {
            babysit_argv.push("--sidecar".into());
            babysit_argv.push(sidecar.into_os_string());
            babysit_argv.push("--sidecar-ready-port".into());
            babysit_argv.push(ready_port.to_string().into());
        }
        babysit_argv.push("--".into());
        babysit_argv.push(binary.clone().into_os_string());
        for a in &args {
            babysit_argv.push(a.into());
        }

        // If a cgroup backend is active, create this service's leaf
        // cgroup and open its `cgroup.procs`. The babysit's pre_exec
        // writes "0" to it, moving the babysit — and the service it
        // execs, plus every descendant — into the leaf, so teardown can
        // `cgroup.kill` the whole subtree (catching daemonized escapees
        // like `epmd` that `killpg` can't reach). Best-effort: any
        // failure logs and falls back to process-group-only for this
        // service. `cgroup_fd_owned` is held open until after spawn so
        // the fd stays valid across the fork.
        let cgroup_fd_owned: Option<std::os::fd::OwnedFd> = match self.backend.base() {
            Some(base) => match super::cgroup::open_leaf_procs(base, entry.name) {
                Ok((_leaf, fd)) => Some(fd),
                Err(e) => {
                    tracing::warn!(
                        service = entry.name,
                        error = %e,
                        "cgroup leaf setup failed; process-group only for this service"
                    );
                    None
                }
            },
            None => None,
        };
        let cgroup_join_fd: Option<i32> = cgroup_fd_owned.as_ref().map(|f| {
            use std::os::fd::AsRawFd;
            f.as_raw_fd()
        });

        let mut cmd = tokio::process::Command::new(&bougie_bin);
        cmd.args(&babysit_argv)
            .arg0("bougie-babysit")
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // SAFETY: `pre_exec` runs in the child after fork and before
        // exec. We do two things, both async-signal-safe: `dup2` the
        // socketpair end onto the babysit's fd 3, then apply the
        // sandbox policy (inherited across the babysit→service exec).
        //
        // `policy` is `None` for catalog entries whose sandbox kind
        // can't be implemented on this platform (today: server's
        // `LightHome` on Linux). Skip the sandbox call in that case
        // so the child runs with the daemon's own privileges.
        #[allow(unsafe_code)]
        unsafe {
            let policy = policy.clone();
            cmd.pre_exec(move || {
                let rc = libc_dup2(child_sock_fd, BABYSIT_CONTROL_FD);
                if rc < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // dup2 clears CLOEXEC on the destination only when
                // oldfd != newfd. If child_sock_fd already *is* fd 3,
                // dup2 is a no-op and the CLOEXEC flag Rust set on the
                // socketpair end survives — closing fd 3 at exec and
                // EOF-ing the babysit's control read. Clear it
                // unconditionally so fd 3 always survives.
                if libc_clear_cloexec(BABYSIT_CONTROL_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Join the service's leaf cgroup BEFORE the sandbox
                // locks down filesystem access (Landlock would block the
                // `/sys/fs/cgroup` write). Writing "0" moves the calling
                // process; descendants inherit the cgroup.
                if let Some(fd) = cgroup_join_fd {
                    let borrowed = rustix::fd::BorrowedFd::borrow_raw(fd);
                    rustix::io::write(borrowed, b"0")
                        .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
                }
                if let Some(policy) = policy.as_ref() {
                    sandbox_run::apply_sandbox(policy)
                        .map_err(|e| std::io::Error::other(format!("sandbox: {e}")))?;
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().wrap_err_with(|| {
            format!(
                "spawning bougie-babysit for {} via {}",
                entry.name,
                binary.display()
            )
        })?;
        // Parent no longer needs the child end; closing it locally
        // means the child holds the only ref. (Doesn't affect the
        // already-dup2'd fd 3 in the child.)
        drop(child_sock);
        // The child has forked; the parent's cgroup.procs fd has served
        // its purpose (the pre_exec wrote through it). Close it.
        drop(cgroup_fd_owned);
        let pid = child.id();

        // Read the babysit's first line: `pgid=<n>\n`. Treat any
        // failure (timeout, parse error, EOF before line) as a spawn
        // failure: SIGKILL the babysit and surface the error.
        parent_sock
            .set_nonblocking(true)
            .wrap_err("setting parent socket non-blocking")?;
        let mut control_sock = tokio::net::UnixStream::from_std(parent_sock)
            .wrap_err("wrapping parent socket as tokio stream")?;
        let service_pgid = match tokio::time::timeout(
            BABYSIT_READY_TIMEOUT,
            read_pgid_line(&mut control_sock),
        )
        .await
        {
            Ok(Ok(pgid)) => pgid,
            Ok(Err(e)) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(eyre!(
                    "babysit for `{}` failed before reporting pgid: {e}",
                    entry.name
                ));
            }
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(eyre!(
                    "babysit for `{}` did not report pgid within {:?}",
                    entry.name,
                    BABYSIT_READY_TIMEOUT
                ));
            }
        };
        // Take the piped fds before we hand the child to the
        // supervisor map; spawn a forwarder per stream so writes land
        // in the LogWriter (which handles rotation). Forwarders exit
        // on EOF when the child closes its pipes.
        if let Some(out) = child.stdout.take() {
            spawn_log_forwarder(out, Arc::clone(&log_writer), entry.name);
        }
        if let Some(err) = child.stderr.take() {
            spawn_log_forwarder(err, Arc::clone(&log_writer), entry.name);
        }

        // Now stamp the running state under a fresh mutable borrow.
        {
            let svc = self
                .services
                .get_mut(entry.name)
                .expect("services map populated in Supervisor::new");
            svc.state = ServiceState::HealthChecking;
            svc.started_at = Some(Instant::now());
            svc.pid = pid;
            svc.child = Some(child);
            svc.service_pgid = Some(service_pgid);
            // The service is its own pgrp leader (setpgid(0,0)), so the
            // leader pid == pgid; record its start-time for reuse checks.
            svc.service_pgid_starttime = proc_starttime(service_pgid);
            svc.control_sock = Some(control_sock);
        }

        // Health-probe with the lock-equivalent (`&mut self`) still
        // held — the caller is expected to be holding `Mutex<Supervisor>`
        // once, so concurrent starts of different services serialise
        // here. Acceptable for Phase 3 (single-service redis). Phase 5+
        // moves the probe off the lock.
        let probe_paths = self.paths.clone();
        let entry_name = entry.name;
        let binding = entry.binding;
        // Hand the child over to wait_for_health so it can short-
        // circuit on early exit (e.g. port-conflict EADDRINUSE) and
        // surface the exit status instead of pretending success
        // because some other process happens to be on the catalog
        // port.
        let mut child_handle = self
            .services
            .get_mut(entry_name)
            .unwrap()
            .child
            .take()
            .expect("BUG: child was just set above");
        let probe_result = wait_for_health(&binding, entry_name, &probe_paths, &mut child_handle).await;
        // Put the child back so check_all / stop can find it.
        self.services.get_mut(entry_name).unwrap().child = Some(child_handle);
        match probe_result {
            Ok(()) => {
                self.services.get_mut(entry_name).unwrap().state = ServiceState::Running;
                Ok(true)
            }
            Err(e) => {
                self.services.get_mut(entry_name).unwrap().state = ServiceState::Failed;
                Err(e)
            }
        }
    }

    /// Stop a running service. SIGTERM, wait up to `STOP_GRACE`, then
    /// SIGKILL. Returns `true` if this call stopped the service;
    /// `false` if it wasn't running.
    pub async fn stop(&mut self, name: &str) -> Result<bool> {
        let entry = catalog::find(name)
            .ok_or_else(|| eyre!("unknown service `{name}`"))?;
        let svc = self
            .services
            .get_mut(entry.name)
            .ok_or_else(|| eyre!("BUG: service `{}` missing from supervisor map", entry.name))?;
        if !matches!(svc.state, ServiceState::Running | ServiceState::HealthChecking | ServiceState::Starting) {
            return Ok(false);
        }
        let (child, pid, control_sock, service_pgid, service_pgid_starttime) = {
            svc.state = ServiceState::Stopping;
            (
                svc.child.take(),
                svc.pid,
                svc.control_sock.take(),
                svc.service_pgid,
                svc.service_pgid_starttime,
            )
        };
        // `svc` goes out of scope here; re-borrow below.

        if let Some(mut child) = child {
            // SIGTERM goes to the babysit, which proxies to the
            // service's pgrp and waits its own grace window. The
            // outer `STOP_GRACE` is sized to outlast `BABYSIT_GRACE_SECS`.
            stop_child(&mut child, pid).await;
        }
        // Drop the control socket only after the babysit has been
        // joined: dropping earlier would EOF the babysit and race
        // with the SIGTERM path above. Dropping after is a no-op for
        // the now-dead babysit.
        drop(control_sock);

        // cgroup backstop: the babysit's graceful killpg above handles
        // the well-behaved process group, but daemonized escapees (e.g.
        // `epmd`) leave it. `cgroup.kill` sweeps the whole leaf, then we
        // remove it. Off the async runtime — the rmdir retry blocks.
        if let Some(leaf) = self.backend.leaf(entry.name) {
            let kill_supported = self.backend.kill_supported();
            let _ = tokio::task::spawn_blocking(move || {
                super::cgroup::kill_and_remove(&leaf, kill_supported);
            })
            .await;
        } else if let Some(pgid) = service_pgid {
            // Process-group backstop: under the ProcessGroup backend
            // there's no cgroup to sweep, so if the babysit died without
            // running its own killpg cleanup — e.g. a foreground Ctrl-C
            // SIGINT took it down before it could — the service's group
            // would be left orphaned. `reap_orphan_group` SIGTERM/SIGKILLs
            // the recorded pgid (with a PID-reuse guard). This mirrors what
            // the ticker's `check_all` does on babysit crash, but `check_all`
            // is torn down the moment shutdown begins, so drain wouldn't
            // otherwise reach it. Blocking (250ms sleep) → spawn_blocking.
            let service = entry.name;
            let _ = tokio::task::spawn_blocking(move || {
                reap_orphan_group(service, pgid, service_pgid_starttime);
            })
            .await;
        }

        if let Some(svc) = self.services.get_mut(entry.name) {
            svc.state = ServiceState::Stopped;
            svc.pid = None;
            svc.service_pgid = None;
            svc.service_pgid_starttime = None;
            svc.started_at = None;
        }
        Ok(true)
    }

    /// Reap any service whose child has exited, then start any
    /// services whose backoff deadline is due.
    ///
    /// Called once per second by the daemon's ticker. Two passes:
    /// first reap+schedule, then respawn — combined under the same
    /// `&mut self` borrow so concurrent IPC handlers see a
    /// consistent supervisor state across the cycle.
    pub async fn check_all(&mut self) {
        // Pass 1: reap exited children, transition Failed, schedule
        // the next restart deadline with exponential backoff.
        let now = Instant::now();
        // Detach the cgroup base from `self` so the loop below can
        // mutably borrow `self.services` while we still build leaf paths.
        let cgroup_base = self.backend.base().map(std::path::Path::to_path_buf);
        let cgroup_kill = self.backend.kill_supported();
        let mut leaves_to_reap: Vec<std::path::PathBuf> = Vec::new();
        for svc in self.services.values_mut() {
            let Some(child) = svc.child.as_mut() else {
                continue;
            };
            match child.try_wait() {
                Ok(Some(_status)) => {
                    // The babysit exited. If it crashed (e.g. SIGKILL
                    // from outside) before cleaning up, the service's
                    // process group may still be alive. Probe the
                    // stored pgid and reap before transitioning state
                    // so the scheduled respawn doesn't double-spawn.
                    if let Some(pgid) = svc.service_pgid {
                        reap_orphan_group(svc.name, pgid, svc.service_pgid_starttime);
                    }
                    // cgroup backstop for the crash path: sweep the leaf
                    // (escapees the killpg reap above can't reach) once
                    // we're done with the `self.services` borrow.
                    if let Some(base) = &cgroup_base {
                        leaves_to_reap.push(super::cgroup::leaf_under(base, svc.name));
                    }
                    let prev_run = svc.started_at.map(|t| now - t);
                    // Reset rule: a Running window of at least
                    // FAILURE_RESET_THRESHOLD means the service is
                    // basically healthy; treat the next failure as
                    // "first." Otherwise this is a continued crash
                    // loop and we keep escalating.
                    let next_count = match prev_run {
                        Some(d) if d >= FAILURE_RESET_THRESHOLD => 1,
                        _ => svc.failure_count.saturating_add(1),
                    };
                    svc.failure_count = next_count;
                    svc.last_failure_at = Some(now);
                    svc.state = ServiceState::Failed;
                    svc.child = None;
                    svc.pid = None;
                    svc.service_pgid = None;
                    svc.service_pgid_starttime = None;
                    svc.control_sock = None;
                    svc.started_at = None;
                    // Schedule a respawn unless we've hit the
                    // attempt cap. Past the cap, leave restart_at
                    // None — the service stays Failed until the
                    // operator manually `services up`s it.
                    svc.restart_at = if next_count <= MAX_RESTART_ATTEMPTS {
                        Some(now + compute_backoff(next_count))
                    } else {
                        None
                    };
                    tracing::warn!(
                        service = svc.name,
                        failure_count = next_count,
                        backoff_ms = svc.restart_at.map(|t| super::duration_to_ms_u64(t - now)),
                        "service crashed; respawn scheduled"
                    );
                }
                Ok(None) => {} // still running
                Err(e) => {
                    tracing::warn!(service = svc.name, error = %e, "try_wait failed");
                }
            }
        }

        // cgroup teardown for any leaves whose service crashed this tick.
        // Off the loop (the `self.services` borrow is released) and off
        // the async runtime (the rmdir retry blocks).
        for leaf in leaves_to_reap {
            let _ = tokio::task::spawn_blocking(move || {
                super::cgroup::kill_and_remove(&leaf, cgroup_kill);
            })
            .await;
        }

        // Pass 2: collect names of services that are Failed with a
        // due restart_at. We can't call self.start() while iterating
        // self.services, so capture names first then act.
        let due_now = Instant::now();
        let due: Vec<&'static str> = self
            .services
            .values()
            .filter(|s| {
                s.state == ServiceState::Failed
                    && s.restart_at.is_some_and(|d| d <= due_now)
            })
            .map(|s| s.name)
            .collect();
        for name in due {
            if let Err(e) = self.start(name).await {
                tracing::warn!(service = name, error = %e, "auto-restart failed; will try again on next backoff tick");
                // start() failed, but it cleared restart_at. Treat
                // this like a fresh crash so backoff keeps growing.
                if let Some(svc) = self.services.get_mut(name) {
                    let now = Instant::now();
                    svc.failure_count = svc.failure_count.saturating_add(1);
                    svc.last_failure_at = Some(now);
                    svc.restart_at = if svc.failure_count <= MAX_RESTART_ATTEMPTS {
                        Some(now + compute_backoff(svc.failure_count))
                    } else {
                        None
                    };
                    svc.state = ServiceState::Failed;
                }
            }
        }
    }
}

/// Exponential backoff: `BASE_BACKOFF * 2^(failure_count - 1)`,
/// capped at `MAX_BACKOFF`. `failure_count` is 1-based (the first
/// failure waits `BASE_BACKOFF`).
fn compute_backoff(failure_count: u32) -> Duration {
    if failure_count == 0 {
        return BASE_BACKOFF;
    }
    let shift = (failure_count - 1).min(32);
    let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let secs = BASE_BACKOFF
        .as_secs()
        .saturating_mul(factor)
        .min(MAX_BACKOFF.as_secs());
    Duration::from_secs(secs)
}

// -------------------- helpers --------------------

/// Per-service `current_dir` override. Returns `None` to inherit
/// bougied's CWD.
///
/// - `opensearch`: its bundled `config/jvm.options` writes the GC log
///   to a relative `logs/` path that the JVM resolves *before*
///   opensearch.yml's `path.logs` is read, so we anchor CWD to the
///   writable data dir so `logs/` resolves under our RW allowlist.
/// - `rabbitmq`: Erlang's BEAM does a `getcwd()` + readdir during
///   boot (the code/loader scans CWD before consulting any -boot
///   path). If bougied was launched from a directory under `$HOME`
///   — e.g. the user ran `bougie start` from their project — the
///   strict sandbox's `ProtectHome::Yes` makes that path unreadable
///   to the child and BEAM aborts with `invalid_current_directory`
///   ("cannot start loader") before it can read its boot script.
///   Anchor CWD to the service data dir, which is in the RW set.
fn render_exec_cwd(entry: &CatalogEntry, paths: &Paths) -> Option<std::path::PathBuf> {
    match entry.name {
        "opensearch" => Some(paths.service_data("opensearch")),
        "rabbitmq" => Some(paths.service_data("rabbitmq")),
        _ => None,
    }
}

/// Per-service env injected into the child before spawn. Returns an
/// empty map when the service runs with no extras. Pinned to a small
/// list of keys so the table is auditable at a glance.
/// For services that need a co-located helper daemon, resolve
/// `(sidecar_exec, ready_port)`. The babysit starts it in the service's
/// process group, ahead of the service. Today only rabbitmq: Erlang's
/// `epmd` must run in-group (not daemonized) so the `killpg`/cgroup
/// teardown reaps it on macOS too, where there's no cgroup backstop.
/// epmd ships in the `erlang` runtime-dep at a stable `bin/epmd`
/// symlink and listens on 4369. Returns `None` (no sidecar) if the
/// erlang dep or epmd can't be resolved — the service still starts,
/// just with rabbitmq's own daemonized epmd (cgroup-reaped on Linux).
fn sidecar_for(entry: &CatalogEntry, paths: &Paths) -> Option<(std::path::PathBuf, u16)> {
    if entry.name != "rabbitmq" {
        return None;
    }
    let erlang = catalog::find("erlang")?;
    let basedir = store_layout::basedir(paths, erlang).ok()?;
    let epmd = basedir.join("bin/epmd");
    epmd.is_file().then_some((epmd, 4369))
}

fn render_exec_env(entry: &CatalogEntry, paths: &Paths) -> Vec<(String, String)> {
    match entry.name {
        "rabbitmq" => {
            // Reuse the provisioner's env-builder so rabbitmqctl and
            // rabbitmq-server agree on RABBITMQ_NODENAME etc. Plus
            // HOME so the Erlang VM can write its `.erlang.cookie`.
            let mut env = super::provisioners::rabbitmq::rabbitmq_env(paths);
            env.push((
                "HOME".into(),
                paths.service_data("rabbitmq").join("home").display().to_string(),
            ));
            env
        }
        "opensearch" => {
            let tmp = paths.service_data("opensearch").join("tmp");
            let conf = paths.service_conf("opensearch");
            // Explicit `OPENSEARCH_JAVA_HOME` short-circuits the
            // platform sniff in `bin/opensearch-env`. Without it,
            // the launcher's `darwin` branch hard-codes
            // `OPENSEARCH_HOME/jdk.app/Contents/Home/bin/java` (the
            // macOS .app-bundle layout) and exits with
            // "could not find java in bundled jdk at ...". Our PBS
            // tarball lays the JDK out at `install/jdk/bin/java`
            // on every platform.
            let java_home = store_layout::basedir(paths, entry)
                .map(|p| p.join("jdk"))
                .unwrap_or_default();
            vec![
                ("OPENSEARCH_JAVA_HOME".into(), java_home.display().to_string()),
                // JNA native-lib extraction + `java.io.tmpdir` write
                // here. `/tmp` is hidden by `ProtectSystem::Strict`.
                ("OPENSEARCH_TMPDIR".into(), tmp.display().to_string()),
                // Contain `$TMPDIR`-honouring temporaries (the JVM,
                // `mktemp`, Linux bash heredocs) inside the already-RW
                // `<datadir>/tmp` (created by `pre_start`) instead of the
                // inherited system temp dir, which Strict hides.
                //
                // NB this does *not* fix the macOS heredoc failure: the
                // `bin/opensearch*` launchers are bash scripts, and macOS
                // `/bin/bash` 3.2 ignores `$TMPDIR` for here-documents
                // (its spooler omits `MT_USETMPDIR`), writing to the
                // compiled `/var/tmp` regardless. That carve-in lives in
                // the sandbox policy (`sandbox::build_strict`), not here.
                ("TMPDIR".into(), tmp.display().to_string()),
                // `opensearch-env` defaults `OPENSEARCH_PATH_CONF` to
                // `$OPENSEARCH_HOME/config`, which is read-only in
                // the store. The provisioner's pre_start hook copies
                // the tarball's config/ into our writable conf dir
                // and rewrites jvm.options to absolute paths; point
                // opensearch at our copy.
                ("OPENSEARCH_PATH_CONF".into(), conf.display().to_string()),
            ]
        }
        _ => Vec::new(),
    }
}

/// Render `exec_args` for a service. Each entry's argv is hand-rolled
/// here rather than templated — services have idiosyncratic flags
/// (mariadb's `--skip-networking`, redis's `--unixsocketperm`, etc.)
/// and the table is short. Fallback `[]` runs the binary with no args.
fn render_exec_args(entry: &CatalogEntry, paths: &Paths) -> Vec<String> {
    match entry.name {
        "redis" => {
            let sock = paths.service_run("redis").join("redis.sock").display().to_string();
            let dir = paths.service_data("redis").display().to_string();
            vec![
                "--port".into(),
                "0".into(),
                "--unixsocket".into(),
                sock,
                "--unixsocketperm".into(),
                "600".into(),
                "--dir".into(),
                dir,
                "--daemonize".into(),
                "no".into(),
                // Send redis's own logging to stderr so our log file
                // captures it (no "logfile" → stderr).
                "--logfile".into(),
                String::new(),
                // Modest defaults — services up under bougied are
                // dev-scale, not prod.
                "--save".into(),
                String::new(),
                "--appendonly".into(),
                "no".into(),
            ]
        }
        "server" => {
            // bougied keeps the per-service server.toml under the
            // service's conf/ dir rather than relying on the user's
            // XDG default — that way `bougie services add server` is
            // a self-contained subsystem that doesn't fight a hand-
            // authored ~/.config/bougie/server.toml. The provisioner
            // (`provisioners::bougie_server`) writes hosts to the
            // same path.
            let cfg = paths.service_conf("server").join("server.toml");
            vec![
                "server".into(),
                "run".into(),
                "--config".into(),
                cfg.display().to_string(),
                "--listen".into(),
                "127.0.0.1:7080".into(),
            ]
        }
        "opensearch" => {
            let data = paths.service_data("opensearch").display().to_string();
            let log = paths.service_log("opensearch").display().to_string();
            // OpenSearch writes JNA-extracted native libs + assorted
            // temporaries under `OPENSEARCH_TMPDIR`. The sandbox hides
            // /tmp (ProtectSystem::Strict), so pin it under the data
            // dir which is already RW. Created by `pre_start`.
            vec![
                format!("-Epath.data={data}"),
                format!("-Epath.logs={log}"),
                // Loopback only — bougie services never bind public
                // addresses (SERVICES.md §6).
                "-Enetwork.host=127.0.0.1".into(),
                // Catalog binding pins :9200. Keep the two in lockstep.
                "-Ehttp.port=9200".into(),
                // No cluster bootstrap — single-node dev mode skips
                // discovery + initial_cluster_manager_nodes ceremony.
                "-Ediscovery.type=single-node".into(),
            ]
        }
        "mariadb" => {
            let data_path = paths.service_data("mariadb");
            let datadir = data_path.display().to_string();
            let sock = paths.service_run("mariadb").join("mariadb.sock").display().to_string();
            // InnoDB writes temporaries during startup. The sandbox
            // hides /tmp (default systemd-style ProtectSystem), so
            // pin them under the already-RW datadir. Best-effort
            // create — mariadbd would create it too, but doing it
            // ahead of time avoids a noisy log line.
            let tmpdir = data_path.join("tmp");
            let _ = std::fs::create_dir_all(&tmpdir);
            // `basedir` is the install root the tarball extracted into;
            // mariadbd reads `share/mariadb/english/errmsg.sys` etc.
            // from there.
            let basedir = store_layout::basedir(paths, entry)
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            vec![
                // Ignore the host's /etc/my.cnf — on CI runners it
                // typically contains MySQL-8-only settings that our
                // bundled mariadbd rejects ("unknown variable
                // 'mysqlx-bind-address=...'"). The only options
                // mariadbd should see are the ones bougied passes
                // explicitly.
                "--no-defaults".into(),
                format!("--basedir={basedir}"),
                format!("--datadir={datadir}"),
                format!("--socket={sock}"),
                format!("--tmpdir={}", tmpdir.display()),
                // Bougie services bind unix sockets only — see
                // SERVICES.md §6. `--skip-networking` ensures mariadbd
                // doesn't also bind 0.0.0.0:3306 by default.
                "--skip-networking".into(),
                // mariadbd writes a slow-query / general-query log
                // into the data dir by default; the dev workflow
                // doesn't need either, and skipping them keeps the
                // datadir small.
                "--general-log=0".into(),
                "--slow-query-log=0".into(),
            ]
        }
        _ => Vec::new(),
    }
}

/// Read everything the child writes to one of its pipes; for each
/// chunk, lock the shared `LogWriter` and append. Exits on EOF (child
/// closed the pipe) or on any non-Interrupted read error. Errors
/// surface only as `tracing::warn!` since the child is the source of
/// truth — failing the forwarder shouldn't fail the service.
fn spawn_log_forwarder<R>(mut reader: R, log: Arc<Mutex<LogWriter>>, service: &'static str)
where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = vec![0u8; 8 * 1024];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => return, // EOF
                Ok(n) => {
                    let mut w = log.lock().await;
                    if let Err(e) = w.write(&buf[..n]) {
                        tracing::warn!(service, error = %e, "writing log chunk");
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    tracing::warn!(service, error = %e, "reading from child pipe");
                    return;
                }
            }
        }
    });
}

#[tracing::instrument(skip_all, fields(service = name))]
async fn wait_for_health(
    binding: &Binding,
    name: &str,
    paths: &Paths,
    child: &mut Child,
) -> Result<()> {
    let timeout = health_timeout_for(name);
    let deadline = Instant::now() + timeout;
    loop {
        // Short-circuit on early child exit BEFORE the TCP/socket
        // probe — otherwise a port-collision (opensearch can't bind
        // 9200 because someone else is on it) would look "Running"
        // to us simply because *some* server answers on 9200.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(eyre!(
                "service `{name}` exited during startup (status {status}); \
                 check `bougie services logs {name}` for the reason"
            ));
        }
        let ok = match binding {
            Binding::UnixSocket { sockname } => {
                let path = paths.service_run(name).join(sockname);
                tokio::net::UnixStream::connect(&path).await.is_ok()
            }
            Binding::Tcp { port } => {
                let addr = ("127.0.0.1", *port);
                tokio::net::TcpStream::connect(addr).await.is_ok()
            }
            Binding::None => true, // runtime-only deps never get probed
        };
        if ok {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(eyre!(
                "service `{name}` did not start accepting connections within {timeout:?}"
            ));
        }
        tokio::time::sleep(HEALTH_POLL).await;
    }
}

/// Read one `pgid=<n>\n` line from the babysit's control socket. Used
/// during start to learn the service's process-group id; bookended by
/// a tokio timeout in the caller.
async fn read_pgid_line(sock: &mut tokio::net::UnixStream) -> std::io::Result<i32> {
    let mut reader = tokio::io::BufReader::new(sock);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "babysit closed control socket before reporting pgid",
        ));
    }
    let trimmed = line.trim_end_matches('\n');
    let pgid = trimmed.strip_prefix("pgid=").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected `pgid=<n>`, got {trimmed:?}"),
        )
    })?;
    pgid.parse::<i32>().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("parsing pgid `{pgid}`: {e}"),
        )
    })
}

/// `dup2` via the libc extern. We avoid pulling `libc` as a direct
/// dependency (rustix already covers what bougie needs), so a single
/// `extern "C"` is the cheapest path.
#[allow(unsafe_code)]
fn libc_dup2(src: i32, dst: i32) -> i32 {
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
    }
    unsafe { dup2(src, dst) }
}

/// Clear all file-descriptor flags (i.e. `FD_CLOEXEC`) on `fd` via
/// `fcntl(F_SETFD, 0)`. Async-signal-safe, used from `pre_exec` to
/// guarantee the babysit control fd survives exec even when `dup2` was
/// a same-fd no-op. Same rationale as [`libc_dup2`] for the bare
/// `extern "C"` instead of a `libc` dependency. `F_SETFD` is `2` on
/// both Linux and macOS.
#[allow(unsafe_code)]
fn libc_clear_cloexec(fd: i32) -> i32 {
    const F_SETFD: i32 = 2;
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    unsafe { fcntl(fd, F_SETFD, 0) }
}

/// Read a process's start-time (jiffies since boot) from
/// `/proc/<pid>/stat` field 22. Returns `None` if the process is gone
/// or the field can't be parsed. The `comm` field (field 2) can contain
/// spaces and parens, so we parse the tail after the last `)`. Linux
/// only — a no-op `None` elsewhere, which makes the reuse guard
/// best-effort on non-Linux Unix.
#[cfg(target_os = "linux")]
fn proc_starttime(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = stat.rfind(')')?;
    // After `comm`, fields resume at field 3 (state); starttime is
    // field 22, i.e. index 19 in the whitespace-split tail.
    stat[rparen + 1..]
        .split_whitespace()
        .nth(19)?
        .parse::<u64>()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn proc_starttime(_pid: i32) -> Option<u64> {
    None
}

/// Best-effort cleanup of a service's process group when the
/// babysit shim has already exited. Called from `check_all` after the
/// supervisor notices an unexpected babysit exit.
///
/// If the group still has members (babysit died before completing its
/// own cleanup), SIGTERM the group, sleep briefly, then SIGKILL.
/// We're inside a 1-second supervisor tick, so the sleep is short by
/// necessity — services that don't shut down on SIGTERM within ~250ms
/// will be killed outright. That's acceptable because the babysit's
/// grace window already gave them their chance under normal flow.
fn reap_orphan_group(service: &str, pgid: i32, leader_starttime: Option<u64>) {
    let Some(pgrp) = rustix::process::Pid::from_raw(pgid) else {
        return;
    };
    // PID-reuse guard: if we recorded the leader's start-time and the
    // pid `pgid` now exists with a *different* start-time, the kernel
    // recycled it onto an unrelated process group — signalling it would
    // kill an innocent group. Skip. (If the leader is simply gone, the
    // probe returns None and we fall through to the existence check,
    // preserving the legitimate orphan-reaping path.)
    if let Some(recorded) = leader_starttime
        && let Some(current) = proc_starttime(pgid)
        && current != recorded
    {
        tracing::warn!(
            service,
            pgid,
            "stored pgid was recycled by an unrelated process; not reaping"
        );
        return;
    }
    // `kill(pgid, 0)` — existence check. `Errno::SRCH` means no
    // members left, which is the normal path (babysit cleaned up
    // before exiting).
    if let Ok(()) = rustix::process::test_kill_process_group(pgrp) {
        tracing::warn!(
            service,
            pgid,
            "babysit exited with live process group; reaping"
        );
        let _ = rustix::process::kill_process_group(pgrp, rustix::process::Signal::TERM);
        std::thread::sleep(Duration::from_millis(250));
        let _ = rustix::process::kill_process_group(pgrp, rustix::process::Signal::KILL);
    } else { /* group already empty — normal path */ }
}

async fn stop_child(child: &mut Child, pid: Option<u32>) {
    if let Some(pid) = pid {
        // SIGTERM via rustix — the bougie codebase already pulls it in.
        // POSIX `pid_t` is `i32`; a pid that doesn't round-trip there
        // shouldn't exist (Linux `pid_max` caps below `i32::MAX`).
        let pid_i32 = match i32::try_from(pid) {
            Ok(p) => p,
            Err(_) => return,
        };
        if let Some(rpid) = rustix::process::Pid::from_raw(pid_i32) {
            let _ = rustix::process::kill_process(rpid, rustix::process::Signal::TERM);
        }
    }
    // Wait up to the grace window. If still running, SIGKILL.
    if let Ok(Ok(_)) = tokio::time::timeout(STOP_GRACE, child.wait()).await {} else {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

// -------------------- topological sort --------------------

/// Kahn's algorithm over catalog `requires` + `after`. Returns the
/// services in the order they should be started; cycles return an
/// error (catalog tests catch typos already, so a cycle is the only
/// remaining failure mode).
///
/// # Panics
///
/// Panics on a BUG: the inner `edges.get_mut`/`indeg.get_mut`
/// calls assume every name in `wanted` got an entry, which is
/// established right before the topological walk.
pub fn compute_start_order(target: &[&str]) -> Result<Vec<&'static str>> {
    // Resolve every target name + transitively every dep.
    let mut wanted: HashSet<&'static str> = HashSet::new();
    for name in target {
        add_with_deps(name, &mut wanted)?;
    }

    // Build in-degree map.
    let mut indeg: BTreeMap<&'static str, usize> =
        wanted.iter().map(|n| (*n, 0)).collect();
    let mut edges: BTreeMap<&'static str, Vec<&'static str>> =
        wanted.iter().map(|n| (*n, Vec::new())).collect();
    for &name in &wanted {
        let entry = catalog::find(name).expect("validated above");
        for &dep in entry.requires.iter().chain(entry.after.iter()) {
            if wanted.contains(dep) {
                edges.get_mut(dep).unwrap().push(name);
                *indeg.get_mut(name).unwrap() += 1;
            }
        }
    }

    let mut ready: Vec<&'static str> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| *n)
        .collect();
    ready.sort_unstable(); // deterministic order

    let mut out = Vec::with_capacity(wanted.len());
    while let Some(n) = ready.pop() {
        out.push(n);
        if let Some(succs) = edges.remove(n) {
            for s in succs {
                let d = indeg.get_mut(s).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.push(s);
                    ready.sort_unstable();
                }
            }
        }
    }
    if out.len() != wanted.len() {
        return Err(eyre!(
            "cycle in service dependency graph involving {:?}",
            wanted.difference(&out.iter().copied().collect()).collect::<Vec<_>>()
        ));
    }
    Ok(out)
}

fn add_with_deps(name: &str, set: &mut HashSet<&'static str>) -> Result<()> {
    let entry = catalog::find(name)
        .ok_or_else(|| eyre!("unknown service `{name}`"))?;
    if set.insert(entry.name) {
        for &dep in entry.requires.iter().chain(entry.after.iter()) {
            add_with_deps(dep, set)?;
        }
    }
    Ok(())
}

/// Shared handle into the supervisor; lives in `DaemonState` and is
/// cloned into the 1-second ticker.
pub type Shared = Arc<Mutex<Supervisor>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_order_solo_service() {
        let order = compute_start_order(&["redis"]).unwrap();
        assert_eq!(order, vec!["redis"]);
    }

    #[test]
    fn start_order_unknown_service_errors() {
        assert!(compute_start_order(&["postgres"]).is_err());
    }

    #[test]
    fn render_exec_args_for_redis_includes_unixsocket() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().into());
        let entry = catalog::find("redis").unwrap();
        let args = render_exec_args(entry, &paths);
        assert!(args.iter().any(|a| a == "--unixsocket"));
        assert!(args.iter().any(|a| a == "--port"));
        // Must include "0" right after "--port"
        let i = args.iter().position(|a| a == "--port").unwrap();
        assert_eq!(args[i + 1], "0");
    }

    #[test]
    fn binary_path_errors_when_tarball_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().into());
        let supervisor = Supervisor::new(paths);
        let entry = catalog::find("redis").unwrap();
        let err = supervisor.binary_path(entry).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("tarball"), "{msg}");
        assert!(msg.contains("redis-8.6.3"), "{msg}");
    }

    #[test]
    fn binary_path_finds_exact_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().into());
        // Lay out an exact-name tarball (no hash suffix).
        std::fs::create_dir_all(paths.store().join("redis-8.6.3/bin")).unwrap();
        std::fs::write(paths.store().join("redis-8.6.3/bin/redis-server"), "fake").unwrap();
        let supervisor = Supervisor::new(paths.clone());
        let entry = catalog::find("redis").unwrap();
        let path = supervisor.binary_path(entry).unwrap();
        assert!(path.ends_with("redis-8.6.3/bin/redis-server"));
    }

    #[test]
    fn binary_path_finds_hashed_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().into());
        // Hash-suffixed tarball, as the index would produce.
        std::fs::create_dir_all(paths.store().join("redis-8.6.3-abc123/bin")).unwrap();
        std::fs::write(paths.store().join("redis-8.6.3-abc123/bin/redis-server"), "fake").unwrap();
        let supervisor = Supervisor::new(paths);
        let entry = catalog::find("redis").unwrap();
        let path = supervisor.binary_path(entry).unwrap();
        assert!(path.to_string_lossy().contains("redis-8.6.3-abc123"));
    }

    #[test]
    fn snapshot_lists_every_catalog_entry_as_stopped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().into());
        let supervisor = Supervisor::new(paths);
        let snap = supervisor.snapshot();
        assert!(snap.iter().any(|s| s.name == "redis" && s.state == ServiceState::Stopped));
        assert!(snap.iter().any(|s| s.name == "mariadb"));
        assert!(snap.iter().any(|s| s.name == "jdk"));
    }

    #[test]
    fn backoff_doubles_each_consecutive_failure() {
        // 1, 2, 4, 8, ... — capped at MAX_BACKOFF.
        assert_eq!(compute_backoff(1), Duration::from_secs(1));
        assert_eq!(compute_backoff(2), Duration::from_secs(2));
        assert_eq!(compute_backoff(3), Duration::from_secs(4));
        assert_eq!(compute_backoff(4), Duration::from_secs(8));
        assert_eq!(compute_backoff(5), Duration::from_secs(16));
    }

    #[test]
    fn backoff_is_capped_at_max() {
        // 2^30 * 1s would overflow real-world patience; cap at MAX_BACKOFF.
        assert_eq!(compute_backoff(20), MAX_BACKOFF);
        assert_eq!(compute_backoff(100), MAX_BACKOFF);
        assert_eq!(compute_backoff(u32::MAX), MAX_BACKOFF);
    }

    #[test]
    fn backoff_for_zero_returns_base() {
        // Edge case — compute_backoff(0) shouldn't panic. It's
        // technically unreachable because we increment to >= 1 before
        // calling, but defending against the off-by-one is cheap.
        assert_eq!(compute_backoff(0), BASE_BACKOFF);
    }

    #[test]
    fn fresh_service_has_no_failure_or_restart_bookkeeping() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().into());
        let supervisor = Supervisor::new(paths);
        let snap = supervisor.snapshot();
        for s in &snap {
            assert_eq!(s.failure_count, 0, "{}: failure_count should be 0", s.name);
            assert!(s.next_restart_ms.is_none(), "{}: no respawn pending", s.name);
        }
    }
}
