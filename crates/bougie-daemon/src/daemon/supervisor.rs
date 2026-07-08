//! Process supervisor: state machine + spawn-with-sandbox + health
//! probes + two-phase stop + topological start order.
//!
//! Phase 3 ships only what redis needs to come online end-to-end.
//! Restart policy, log rotation, and the broader catalog provisioner
//! dispatch land in subsequent phases (5–10).

use super::catalog::{self, Binding, CatalogEntry};
use super::endpoint;
use super::instance::{Instance, InstanceId};
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

/// How often a `Running`/`Unhealthy` service is re-probed by the
/// continuous health loop (driven off-lock by the daemon ticker).
const HEALTH_INTERVAL: Duration = Duration::from_secs(5);

/// Consecutive failed health probes before a `Running` service is
/// declared broken, torn down, and restarted. At [`HEALTH_INTERVAL`]
/// this is ~15s of sustained failure — long enough that a single
/// transient blip (a probe timing out under load) won't flap a healthy
/// service, short enough to catch a genuinely wedged one quickly.
const HEALTH_FAILURE_THRESHOLD: u32 = 3;

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
    /// A live process (has a child, started successfully) that is now
    /// *failing* its continuous health probe. Behaves like `Running`
    /// for stop/teardown purposes — it's a real process — but signals
    /// the service isn't actually serving. After
    /// [`HEALTH_FAILURE_THRESHOLD`] consecutive misses the supervisor
    /// tears it down and restarts it (see [`Supervisor::fail_unhealthy`]).
    Unhealthy,
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
    /// Concrete resolved version this instance runs at. With the catalog
    /// still single-version this is the catalog default, but the map is
    /// keyed by `(name, version)` so a second version of one service is a
    /// distinct slot.
    pub version: String,
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
    /// Consecutive failed continuous-health probes. Reset to 0 on any
    /// passing probe. At `HEALTH_FAILURE_THRESHOLD` the service is torn
    /// down + restarted. Surfaced in `bougie service status`.
    pub health_misses: u32,
    /// When the next continuous health probe is due. `Some` only for a
    /// live, probe-able service (`Running`/`Unhealthy`); the ticker's
    /// `health_due` checks it. `None` clears it from the rotation.
    pub next_health_at: Option<Instant>,
    /// True while an off-lock probe for this service is in flight, so the
    /// ticker doesn't stack a second probe on top of a slow one.
    pub health_inflight: bool,
    /// When the most recent probe last passed. Diagnostic only.
    pub last_health_ok: Option<Instant>,
}

impl ManagedService {
    fn new(name: &'static str, version: impl Into<String>) -> Self {
        Self {
            name,
            version: version.into(),
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
            health_misses: 0,
            next_health_at: None,
            health_inflight: false,
            last_health_ok: None,
        }
    }

    /// This slot's `(name, version)` identity. Callers derive the map key
    /// from it via [`Instance::id`].
    fn instance(&self) -> Instance {
        Instance::new(self.name, &self.version)
    }
}

/// Snapshot returned by the IPC `status` method. Stable shape; the
/// daemon owns the live state via `Supervisor`.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    /// The instance's resolved version — distinguishes two instances of
    /// one service in the status list.
    pub version: String,
    pub state: ServiceState,
    pub pid: Option<u32>,
    pub uptime_ms: Option<u64>,
    pub binding: Binding,
    /// Consecutive failure count. `0` when the service hasn't
    /// crashed recently. Inspected by `bougie service daemon
    /// status` so users can see backoff state.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub failure_count: u32,
    /// Milliseconds until the supervisor will respawn this service.
    /// `Some` only while state == Failed and a respawn is scheduled
    /// (i.e. `failure_count` <= `MAX_RESTART_ATTEMPTS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_restart_ms: Option<u64>,
    /// Consecutive failed continuous-health probes. `0` for a healthy
    /// service; non-zero means it's failing its probe and counting down
    /// to a teardown-and-restart at `HEALTH_FAILURE_THRESHOLD`.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub health_misses: u32,
    /// Consecutive-miss threshold at which an `Unhealthy` service is
    /// restarted. Surfaced so the CLI can render `2/3` without baking the
    /// constant into the client.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub health_threshold: u32,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

/// A spawned-but-not-yet-healthy service, handed back by
/// [`Supervisor::spawn_service`] to be probed *off* the supervisor lock.
///
/// It owns the babysit [`Child`]: while the service sits in
/// `HealthChecking` its `child` slot in the supervisor map is `None`, so
/// [`Supervisor::check_all`] skips it (it only reaps services whose child
/// is present) and the health probe — up to 90s for opensearch/rabbitmq —
/// no longer blocks `status` or the 1s reaper behind `Mutex<Supervisor>`.
/// See [`start_service`].
#[derive(Debug)]
pub struct PendingHealth {
    instance: Instance,
    paths: Paths,
    child: Child,
}

impl PendingHealth {
    /// Probe until the service is healthy, exits early, or its
    /// per-service deadline elapses. Runs without the supervisor lock.
    async fn wait_healthy(&mut self) -> Result<()> {
        wait_for_health(&self.instance, &self.paths, &mut self.child).await
    }
}

/// Outcome of [`Supervisor::spawn_service`] — the under-lock half of a
/// start.
#[derive(Debug)]
pub enum SpawnOutcome {
    /// The service was already Starting/HealthChecking/Running; no-op.
    AlreadyRunning,
    /// A fresh babysit was spawned and stamped `HealthChecking`; the
    /// caller must probe the [`PendingHealth`] off-lock and then call
    /// [`Supervisor::finalize_start`]. Boxed: `PendingHealth` dwarfs the
    /// unit `AlreadyRunning` variant.
    Spawned(Box<PendingHealth>),
}

/// Outcome of [`Supervisor::finalize_start`] — the under-lock half that
/// resolves a `HealthChecking` service once its off-lock probe returns.
#[derive(Debug)]
pub enum StartFinalize {
    /// Probe succeeded; the service is now `Running`.
    Started,
    /// Probe failed; the service is `Failed` (the dead/wedged babysit was
    /// put back in the map for `check_all` to reap+reschedule).
    Failed(eyre::Report),
    /// The service left `HealthChecking` while the probe ran off-lock — a
    /// racing `stop`/drain. We did not resurrect it; the now-stale babysit
    /// [`Child`] is handed back for the caller to reap off-lock.
    Superseded(Child),
}

/// Outcome of [`Supervisor::record_health`] — how a continuous-health
/// probe result moved the service's state.
#[derive(Debug, PartialEq, Eq)]
pub enum HealthOutcome {
    /// Probe passed (or recovered an `Unhealthy` service to `Running`).
    Healthy,
    /// Probe failed but the consecutive-miss count is still under
    /// [`HEALTH_FAILURE_THRESHOLD`]; the service is now `Unhealthy`.
    Degraded,
    /// The consecutive-miss threshold was hit — the caller (the ticker)
    /// must tear the wedged service down and reschedule it via
    /// [`Supervisor::fail_unhealthy`].
    Breach,
    /// The service moved out of a probe-able state (`Running`/`Unhealthy`)
    /// while the probe ran off-lock — stopped, crashed, or restarted.
    /// The stale result is discarded.
    Gone,
}

#[derive(Debug)]
pub struct Supervisor {
    /// Keyed by instance id (`<name>-<version>`), created lazily on first
    /// `up`. An empty map means nothing has been requested yet — a service
    /// declared-but-never-upped simply has no slot (treated as Stopped).
    services: HashMap<InstanceId, ManagedService>,
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
        // Instances are created lazily on first `up` — the map starts
        // empty. (Pre-seeding a Stopped slot per catalog entry made sense
        // when a service was a name-singleton; now that two versions of one
        // service can coexist, there's no single slot to pre-seed.)
        let services = HashMap::new();
        let backend = super::cgroup::detect(&paths.state());
        tracing::info!(
            backend = backend.label(),
            svc_root = backend.svc_root().map(|p| p.display().to_string()),
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
    /// the flock singleton guarantees no other live instance *for this
    /// home*, and the svc root is namespaced by the same home identity
    /// (see `cgroup::svc_dir_name`), so any leaves present are orphans
    /// safe to reap; daemons for other `BOUGIE_HOME`s sharing this
    /// session's delegated cgroup keep their own namespaces. No-op under
    /// the process-group backend. Runs on the blocking pool.
    pub async fn reap_stale_leaves(&self) {
        let Some(root) = self.backend.svc_root().map(std::path::Path::to_path_buf) else {
            return;
        };
        let kill_supported = self.backend.kill_supported();
        let reaped = tokio::task::spawn_blocking(move || {
            super::cgroup::reap_stale_leaves(&root, kill_supported)
        })
        .await
        .unwrap_or_default();
        if !reaped.is_empty() {
            tracing::warn!(?reaped, "reaped leftover service cgroups from a dead bougied");
        }
    }

    /// Best-effort removal of this daemon's namespaced service-cgroup
    /// dir, for clean shutdown after the drain. A single `rmdir`: the
    /// kernel refuses to remove a cgroup that still has members or
    /// children, so this can never take a live leaf with it. No-op under
    /// the process-group backend.
    pub fn remove_svc_root(&self) {
        if let Some(root) = self.backend.svc_root() {
            let _ = std::fs::remove_dir(root);
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
                    version: svc.version.clone(),
                    state: svc.state,
                    pid: svc.pid,
                    uptime_ms: svc
                        .started_at
                        .map(|t| super::duration_to_ms_u64(t.elapsed())),
                    binding: entry.binding,
                    failure_count: svc.failure_count,
                    next_restart_ms,
                    health_misses: svc.health_misses,
                    // Only meaningful while actively missing probes; keep
                    // the snapshot quiet otherwise (skip_serializing_if).
                    health_threshold: if svc.health_misses > 0 {
                        HEALTH_FAILURE_THRESHOLD
                    } else {
                        0
                    },
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
        out
    }

    /// Resolve the on-disk path of a service's main binary. Thin
    /// wrapper over `store_layout::binary` so the supervisor and the
    /// per-service provisioners agree on where each service lives.
    fn binary_path(&self, entry: &CatalogEntry, version: &str) -> Result<std::path::PathBuf> {
        store_layout::binary(&self.paths, entry, version)
    }

    /// Resolve (and persist) the effective TCP endpoint for an instance:
    /// reuse a recorded endpoint's ports where still bindable (sticky),
    /// else the catalog defaults, else scan upward for free ports. Writes
    /// `endpoint.json`. Returns `None` for socket-only / `Binding::None`
    /// services — they can't collide on a port, and their coexistence
    /// comes from the version-keyed socket path, not a port.
    fn resolve_endpoint(
        &self,
        entry: &CatalogEntry,
        version: &str,
    ) -> Result<Option<endpoint::ServiceEndpoint>> {
        let Binding::Tcp { port: default_primary } = entry.binding else {
            return Ok(None);
        };
        let ep_path = self.paths.service_endpoint(entry.name, version);
        let recorded = endpoint::ServiceEndpoint::load(&ep_path)?;

        // Ports claimed in this pass, so a multi-port service can't pick
        // the same number twice. Each `allocate_port` gets a fresh closure
        // (dropped before the following `push`), so `claimed` can grow
        // between calls.
        let mut claimed: Vec<u16> = Vec::new();

        let primary = super::ports::allocate_port(
            default_primary,
            recorded.as_ref().map(|e| e.primary),
            |p| !claimed.contains(&p) && !super::ports::port_in_use(p),
        )
        .ok_or_else(|| {
            eyre!(
                "no free port near {default_primary} for `{}` (scanned {} above the default)",
                entry.name,
                super::ports::PORT_SCAN_SPAN,
            )
        })?;
        claimed.push(primary);
        if primary != default_primary {
            tracing::warn!(
                service = entry.name,
                default = default_primary,
                chosen = primary,
                holder = %super::ports::describe_holder(default_primary),
                "catalog port taken; relocating to a free port",
            );
        }

        let mut ep = endpoint::ServiceEndpoint::new(primary);
        for &(label, default) in secondary_ports(entry.name) {
            let recorded_extra = recorded.as_ref().and_then(|e| e.extra_port(label));
            let chosen = super::ports::allocate_port(default, recorded_extra, |p| {
                !claimed.contains(&p) && !super::ports::port_in_use(p)
            })
            .ok_or_else(|| eyre!("no free {label} port near {default} for `{}`", entry.name))?;
            claimed.push(chosen);
            if chosen != default {
                tracing::warn!(
                    service = entry.name,
                    port = label,
                    default,
                    chosen,
                    "secondary port taken; relocating",
                );
            }
            ep.extra.insert(label.to_owned(), chosen);
        }

        ep.save(&ep_path)?;
        Ok(Some(ep))
    }

    /// Spawn a service instance's babysit and stamp it `HealthChecking`,
    /// *without* running the health probe — the caller ([`start_service`])
    /// drops the lock and probes the returned [`PendingHealth`] off-lock,
    /// then re-locks for [`Supervisor::finalize_start`]. Splitting the
    /// start this way keeps the up-to-90s probe from blocking `status` and
    /// the reaper behind `Mutex<Supervisor>`.
    ///
    /// The `(name, version)` instance slot is created lazily on the first
    /// `up` — there is no pre-seeded map, so a missing slot is normal, not
    /// a bug. The `.expect`/`.unwrap` on `services.get_mut(&id)` below are
    /// safe only because this method inserts the slot at the top and holds
    /// the lock throughout; nothing removes it mid-call.
    ///
    /// Returns [`SpawnOutcome::AlreadyRunning`] if the instance was already
    /// Starting/HealthChecking/Running.
    pub async fn spawn_service(&mut self, inst: &Instance) -> Result<SpawnOutcome> {
        let entry = catalog::find(&inst.name)
            .ok_or_else(|| eyre!("unknown service `{}`", inst.name))?;
        let version = inst.version.as_str();
        let id = inst.id();
        // Lazy creation: the first `up` for this (name, version) instance
        // materializes its slot. There is no pre-seeded Stopped slot to
        // find anymore — the map holds only instances that were requested.
        self.services
            .entry(id.clone())
            .or_insert_with(|| ManagedService::new(entry.name, version));
        // Idempotence check via an immutable borrow that ends here.
        {
            let svc = self.services.get(&id).expect("slot just inserted above");
            if matches!(
                svc.state,
                ServiceState::Running
                    | ServiceState::Unhealthy
                    | ServiceState::HealthChecking
                    | ServiceState::Starting
            ) {
                return Ok(SpawnOutcome::AlreadyRunning);
            }
        }
        // Clear any pending auto-restart deadline — we're starting
        // now, either from operator action or from the ticker firing
        // a due restart. `failure_count` carries over until either a
        // successful sustained run resets it (handled in check_all)
        // or another failure increments it.
        if let Some(svc) = self.services.get_mut(&id) {
            svc.restart_at = None;
        }

        // Resolve the effective TCP endpoint BEFORE spawning: allocate a
        // free port when the catalog default is already taken — by the
        // developer's own service or by a sibling instance (two search
        // engines both want 9200) — reusing a previously-recorded port
        // stickily. Persisted to endpoint.json so the exec args, the
        // health probe, and offline consumers (`bougie run` env,
        // `credentials`) all agree on where the service actually landed.
        //
        // This replaces the old hard-fail-on-occupied-port: rather than
        // refuse, we bind our *own* free port, which also closes the
        // masquerade hazard the hard-fail guarded against — we can never
        // health-probe onto someone else's live service.
        let endpoint = self.resolve_endpoint(entry, version)?;

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
        let binary = self.binary_path(entry, version)?;
        let args = render_exec_args(entry, version, &self.paths, endpoint.as_ref());
        let log_path = self.paths.service_log_file(entry.name, version);
        // Open the LogWriter eagerly — confirms the parent dir is
        // writable before we fork a child. Wrap in Arc<Mutex<…>> so
        // the two stdio forwarder tasks (stdout, stderr) can share
        // it; rotation under live writes is then serialised by the
        // mutex.
        let log_writer = LogWriter::open(log_path)
            .wrap_err_with(|| format!("opening log writer for {}", entry.name))?;
        let log_writer = Arc::new(Mutex::new(log_writer));

        let env = render_exec_env(entry, version, &self.paths, endpoint.as_ref());
        let cwd = render_exec_cwd(entry, version, &self.paths);

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
        let cgroup_fd_owned: Option<std::os::fd::OwnedFd> = match self.backend.svc_root() {
            Some(root) => match super::cgroup::open_leaf_procs(root, entry.name) {
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
                .get_mut(&id)
                .expect("instance slot inserted at the top of spawn_service");
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

        // Take the child back out of the map and hand it to the caller
        // for an *off-lock* health probe (up to 90s for opensearch /
        // rabbitmq). While the service sits in `HealthChecking` its
        // `child` slot is `None`, so `check_all` skips it — it only reaps
        // services whose child is present — and the long probe no longer
        // blocks `status` or the reaper behind `Mutex<Supervisor>`. The
        // caller probes the returned `PendingHealth`, then re-locks
        // briefly for `finalize_start`. `PendingHealth::wait_healthy`
        // short-circuits on early child exit (e.g. port-conflict
        // EADDRINUSE) so a stray process already on the catalog port
        // can't masquerade as a healthy start.
        let child = self
            .services
            .get_mut(&id)
            .unwrap()
            .child
            .take()
            .expect("BUG: child was just set above");
        Ok(SpawnOutcome::Spawned(Box::new(PendingHealth {
            instance: inst.clone(),
            paths: self.paths.clone(),
            child,
        })))
    }

    /// Resolve a service's `HealthChecking` state once its off-lock probe
    /// (see [`PendingHealth::wait_healthy`]) returns. Held under the
    /// supervisor lock, but only briefly. `child` is the babysit handle
    /// the probe owned; `probe` is its result.
    ///
    /// # Panics
    ///
    /// Panics if `name` is not in the supervisor map (a catalog/BUG
    /// invariant, same as [`Supervisor::spawn_service`]).
    pub fn finalize_start(
        &mut self,
        inst: &Instance,
        child: Child,
        probe: Result<()>,
    ) -> StartFinalize {
        // The instance slot exists — `spawn_service` created it and we're
        // resolving that same spawn. A racing `stop` may have changed its
        // state (handled below) but never removes the slot.
        let Some(svc) = self.services.get_mut(&inst.id()) else {
            return StartFinalize::Superseded(child);
        };
        // If the service left `HealthChecking` while we probed off-lock —
        // a racing `stop`/`down` or a daemon drain — honor the newer
        // state instead of resurrecting it. `stop` already tore the group
        // down via the pgid/cgroup backstop (the child was `None` in the
        // map) and dropped the control socket, so hand the now-stale
        // babysit handle back for the caller to reap off-lock.
        if svc.state != ServiceState::HealthChecking {
            return StartFinalize::Superseded(child);
        }
        // Put the child back so `check_all` / `stop` can find it again.
        svc.child = Some(child);
        match probe {
            Ok(()) => {
                svc.state = ServiceState::Running;
                // Arm the continuous-health clock now that it's serving.
                let now = Instant::now();
                svc.health_misses = 0;
                svc.last_health_ok = Some(now);
                svc.next_health_at = Some(now + HEALTH_INTERVAL);
                svc.health_inflight = false;
                StartFinalize::Started
            }
            Err(e) => {
                svc.state = ServiceState::Failed;
                svc.next_health_at = None;
                svc.health_inflight = false;
                StartFinalize::Failed(e)
            }
        }
    }

    /// Stop a running service. SIGTERM, wait up to `STOP_GRACE`, then
    /// SIGKILL. Returns `true` if this call stopped the service (or
    /// disarmed a pending auto-restart); `false` if there was nothing to
    /// stop.
    ///
    /// `Failed` counts as stoppable: a crashed service holds an armed
    /// `restart_at` that `check_all` would otherwise honor, resurrecting
    /// a service the user explicitly took down; and a probe-timeout can
    /// leave a *live* child parked under `Failed` (see `finalize_start`)
    /// that only this path can kill. Both are handled by running the full
    /// teardown below and clearing the crash-backoff bookkeeping.
    pub async fn stop(&mut self, inst: &Instance) -> Result<bool> {
        let entry = catalog::find(&inst.name)
            .ok_or_else(|| eyre!("unknown service `{}`", inst.name))?;
        let id = inst.id();
        // An unknown / never-upped instance has no slot — nothing to stop.
        let Some(svc) = self.services.get_mut(&id) else {
            return Ok(false);
        };
        if !matches!(
            svc.state,
            ServiceState::Running
                | ServiceState::Unhealthy
                | ServiceState::HealthChecking
                | ServiceState::Starting
                | ServiceState::Failed
        ) {
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

        let had_child = child.is_some();
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

        // When we had no babysit handle to SIGTERM + await — the service
        // was mid-`HealthChecking`, so its handle is off with the off-lock
        // probe (`PendingHealth`) and the map's `child` slot is `None` —
        // the control-socket EOF just above is what triggers the babysit's
        // graceful `killpg` teardown. Give that its grace window (poll the
        // service group until it drains) before the hard backstop below
        // SIGKILLs the group out from under it. Without this a service
        // stopped during its first-boot probe (e.g. mariadb initializing
        // its datadir) is killed with ~no grace, risking corruption.
        if !had_child
            && let Some(pgid) = service_pgid
        {
            wait_for_group_drain(pgid, STOP_GRACE).await;
        }

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

        if let Some(svc) = self.services.get_mut(&id) {
            svc.state = ServiceState::Stopped;
            svc.pid = None;
            svc.service_pgid = None;
            svc.service_pgid_starttime = None;
            svc.started_at = None;
            // A stopped service leaves the health rotation.
            svc.next_health_at = None;
            svc.health_inflight = false;
            svc.health_misses = 0;
            // An explicit stop resets crash-backoff state: clear any armed
            // restart deadline so `check_all` won't resurrect the service,
            // and reset the failure count so a later manual start gets a
            // fresh backoff budget.
            svc.restart_at = None;
            svc.failure_count = 0;
        }
        Ok(true)
    }

    /// Reap any service whose child has exited, then return the instances
    /// whose backoff deadline is now due for an auto-restart.
    ///
    /// Called once per second by the daemon's ticker. Two passes: first
    /// reap+schedule under this `&mut self` borrow, then collect the due
    /// restarts. The restarts themselves are *not* performed here — the
    /// ticker drives each one through [`start_service`] off the lock, so a
    /// 90s health probe can't stall reaping or `status`. The due deadlines
    /// are cleared on collection so the next tick doesn't re-issue a
    /// restart that's already in flight.
    pub async fn check_all(&mut self) -> Vec<Instance> {
        // Pass 1: reap exited children, transition Failed, schedule
        // the next restart deadline with exponential backoff.
        let now = Instant::now();
        // Detach the cgroup svc root from `self` so the loop below can
        // mutably borrow `self.services` while we still build leaf paths.
        let cgroup_root = self.backend.svc_root().map(std::path::Path::to_path_buf);
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
                    if let Some(root) = &cgroup_root {
                        leaves_to_reap.push(super::cgroup::leaf_under(root, svc.name));
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
                    // Out of the health rotation until it's Running again.
                    svc.next_health_at = None;
                    svc.health_inflight = false;
                    svc.health_misses = 0;
                    // Schedule a respawn unless we've hit the
                    // attempt cap. Past the cap, leave restart_at
                    // None — the service stays Failed until the
                    // operator manually `service up`s it.
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

        // Pass 2: collect services that are Failed with a due restart
        // deadline. We don't restart them here — that would hold the
        // supervisor lock across each service's (up-to-90s) health probe,
        // re-introducing the very stall this split fixes. Clear the
        // deadline on collection so the next tick doesn't re-issue a
        // restart that's already in flight, and hand the names back to the
        // ticker, which drives each restart through `start_service` off
        // the lock.
        let due_now = Instant::now();
        let due: Vec<Instance> = self
            .services
            .values()
            .filter(|s| {
                s.state == ServiceState::Failed
                    && s.restart_at.is_some_and(|d| d <= due_now)
            })
            .map(ManagedService::instance)
            .collect();
        for inst in &due {
            if let Some(svc) = self.services.get_mut(&inst.id()) {
                svc.restart_at = None;
            }
        }
        due
    }

    /// Record that an off-lock auto-restart (driven by the ticker via
    /// [`start_service`]) failed during the *spawn* phase: the service is
    /// `Failed` with no child and no pending deadline, so nothing would
    /// otherwise retry it. Bump the failure count and schedule the next
    /// backoff. A *probe*-phase failure instead leaves the dead babysit in
    /// the map for the next [`Supervisor::check_all`] tick to
    /// reap+reschedule, so this no-ops when a child is already present.
    pub fn note_restart_failure(&mut self, inst: &Instance) {
        let Some(svc) = self.services.get_mut(&inst.id()) else {
            return;
        };
        if svc.child.is_some() || svc.restart_at.is_some() {
            // Probe-failure path (check_all will reap+reschedule) or a
            // restart already re-armed elsewhere; leave it alone.
            return;
        }
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

    /// Collect `Running`/`Unhealthy` services whose continuous-health
    /// probe is due, marking each in-flight so the next tick won't stack a
    /// second probe on a slow one. The ticker runs each probe *off* the
    /// lock (a probe can take seconds) and reports back via
    /// [`Supervisor::record_health`] — same off-lock discipline as the
    /// start-time probe. Services with no real binding (runtime-only deps)
    /// are never probed.
    pub fn health_due(&mut self) -> Vec<Instance> {
        let now = Instant::now();
        let mut due = Vec::new();
        for svc in self.services.values_mut() {
            if !matches!(svc.state, ServiceState::Running | ServiceState::Unhealthy) {
                continue;
            }
            if svc.health_inflight {
                continue;
            }
            if catalog::find(svc.name).is_some_and(|e| matches!(e.binding, Binding::None)) {
                continue;
            }
            if svc.next_health_at.is_some_and(|t| t <= now) {
                svc.health_inflight = true;
                due.push(svc.instance());
            }
        }
        due
    }

    /// Record a continuous-health probe result and advance the service's
    /// health state. Fast + under the lock — the probe itself already ran
    /// off-lock. Reschedules the next probe and returns what the caller
    /// must do next (see [`HealthOutcome`]).
    pub fn record_health(&mut self, inst: &Instance, ok: bool) -> HealthOutcome {
        let now = Instant::now();
        let name = inst.name.as_str();
        let Some(svc) = self.services.get_mut(&inst.id()) else {
            return HealthOutcome::Gone;
        };
        // A racing stop/crash/restart moved it out of a probe-able state
        // while the probe ran off-lock — discard the stale result.
        if !matches!(svc.state, ServiceState::Running | ServiceState::Unhealthy) {
            svc.health_inflight = false;
            return HealthOutcome::Gone;
        }
        svc.health_inflight = false;
        svc.next_health_at = Some(now + HEALTH_INTERVAL);
        if ok {
            svc.health_misses = 0;
            svc.last_health_ok = Some(now);
            if svc.state == ServiceState::Unhealthy {
                svc.state = ServiceState::Running;
                tracing::info!(service = name, "service recovered; health checks passing again");
            }
            return HealthOutcome::Healthy;
        }
        svc.health_misses = svc.health_misses.saturating_add(1);
        if svc.health_misses >= HEALTH_FAILURE_THRESHOLD {
            // Out of the rotation; `fail_unhealthy` re-arms on restart.
            // Leave it `Unhealthy` so that method's guard fires.
            svc.next_health_at = None;
            svc.state = ServiceState::Unhealthy;
            return HealthOutcome::Breach;
        }
        if svc.state == ServiceState::Running {
            svc.state = ServiceState::Unhealthy;
            tracing::warn!(
                service = name,
                misses = svc.health_misses,
                threshold = HEALTH_FAILURE_THRESHOLD,
                "service failing health checks"
            );
        }
        HealthOutcome::Degraded
    }

    /// Tear down a service that failed its health probe past the
    /// threshold (a [`HealthOutcome::Breach`]) and schedule a backoff
    /// respawn. The process is alive-but-wedged, so we graceful-stop it
    /// (reusing [`Supervisor::stop`]'s SIGTERM→grace→cgroup teardown) and
    /// then re-stamp it as a crash-equivalent failure — bumping
    /// `failure_count` with the same `FAILURE_RESET_THRESHOLD` rule and
    /// scheduling (or giving up on) a restart exactly like the crash arm
    /// of [`Supervisor::check_all`]. The next `check_all` tick's `due`
    /// collection then performs the respawn off-lock.
    ///
    /// Held under the lock across the teardown — the same brief grace
    /// window `bougie down`/`restart` already hold it for. Breaches are
    /// rare, so the cost to the 1s reaper is bounded and infrequent.
    pub async fn fail_unhealthy(&mut self, inst: &Instance) {
        let id = inst.id();
        let name = inst.name.as_str();
        // Capture the prior Running window AND failure count before `stop`
        // clears them (`stop` zeroes `failure_count` + `started_at`), so
        // the escalation/reset rule matches the crash path — otherwise a
        // health breach could never advance past failure #1.
        let (prev_run, prev_count) = self
            .services
            .get(&id)
            .map(|s| (s.started_at.map(|t| t.elapsed()), s.failure_count))
            .unwrap_or((None, 0));
        // Only act if it's still the `Unhealthy` service we flagged — a
        // racing stop/restart may have moved it on.
        if !matches!(
            self.services.get(&id).map(|s| s.state),
            Some(ServiceState::Unhealthy)
        ) {
            return;
        }
        // Graceful teardown of the wedged group (sets state Stopped).
        let _ = self.stop(inst).await;
        // Re-stamp as a crash-equivalent failure so the existing
        // backoff/give-up machinery respawns it.
        let now = Instant::now();
        let Some(svc) = self.services.get_mut(&id) else {
            return;
        };
        let next_count = match prev_run {
            Some(d) if d >= FAILURE_RESET_THRESHOLD => 1,
            _ => prev_count.saturating_add(1),
        };
        svc.failure_count = next_count;
        svc.last_failure_at = Some(now);
        svc.state = ServiceState::Failed;
        svc.health_misses = 0;
        svc.restart_at = if next_count <= MAX_RESTART_ATTEMPTS {
            Some(now + compute_backoff(next_count))
        } else {
            None
        };
        tracing::warn!(
            service = name,
            failure_count = next_count,
            gave_up = svc.restart_at.is_none(),
            "service failed continuous health checks; torn down, respawn scheduled"
        );
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
fn render_exec_cwd(entry: &CatalogEntry, version: &str, paths: &Paths) -> Option<std::path::PathBuf> {
    match entry.name {
        "opensearch" => Some(paths.service_data("opensearch", version)),
        "rabbitmq" => Some(paths.service_data("rabbitmq", version)),
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
    let basedir = store_layout::basedir(paths, erlang, &erlang.version).ok()?;
    let epmd = basedir.join("bin/epmd");
    epmd.is_file().then_some((epmd, 4369))
}

/// OpenSearch's default transport/cluster port (9300). Not modelled by
/// the catalog `Binding` (which holds one port), so it rides in
/// `endpoint.json` under the `transport` label — and, like the HTTP
/// port, gets relocated when a sibling instance or a foreign process
/// already holds it.
const OPENSEARCH_TRANSPORT_PORT: u16 = 9300;

/// Named secondary TCP ports a service binds alongside its primary
/// `Binding::Tcp` port. Each is allocated (and relocated on conflict)
/// independently by [`Supervisor::resolve_endpoint`] and recorded in
/// `endpoint.json` under its label.
fn secondary_ports(name: &str) -> &'static [(&'static str, u16)] {
    match name {
        // Web UI / REST API; SMTP is the primary binding.
        "mailpit" => &[("http", catalog::MAILPIT_HTTP_PORT)],
        // Transport port, bound even in single-node mode.
        "opensearch" => &[("transport", OPENSEARCH_TRANSPORT_PORT)],
        _ => &[],
    }
}

fn render_exec_env(
    entry: &CatalogEntry,
    version: &str,
    paths: &Paths,
    endpoint: Option<&endpoint::ServiceEndpoint>,
) -> Vec<(String, String)> {
    match entry.name {
        "rabbitmq" => {
            // Reuse the provisioner's env-builder so rabbitmqctl and
            // rabbitmq-server agree on RABBITMQ_NODENAME etc. Plus
            // HOME so the Erlang VM can write its `.erlang.cookie`. The
            // AMQP listener binds the effective (possibly relocated)
            // port.
            let node_port = endpoint.map_or(5672, |e| e.primary);
            let mut env = super::provisioners::rabbitmq::rabbitmq_env(paths, node_port);
            env.push((
                "HOME".into(),
                paths.service_data("rabbitmq", version).join("home").display().to_string(),
            ));
            env
        }
        "opensearch" => {
            let tmp = paths.service_data("opensearch", version).join("tmp");
            let conf = paths.service_conf("opensearch", version);
            // Explicit `OPENSEARCH_JAVA_HOME` short-circuits the
            // platform sniff in `bin/opensearch-env`. Without it,
            // the launcher's `darwin` branch hard-codes
            // `OPENSEARCH_HOME/jdk.app/Contents/Home/bin/java` (the
            // macOS .app-bundle layout) and exits with
            // "could not find java in bundled jdk at ...". Our PBS
            // tarball lays the JDK out at `install/jdk/bin/java`
            // on every platform.
            let java_home = store_layout::basedir(paths, entry, version)
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
fn render_exec_args(
    entry: &CatalogEntry,
    version: &str,
    paths: &Paths,
    endpoint: Option<&endpoint::ServiceEndpoint>,
) -> Vec<String> {
    match entry.name {
        "redis" => {
            let sock = paths.service_run("redis", version).join("redis.sock").display().to_string();
            let dir = paths.service_data("redis", version).display().to_string();
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
            // XDG default — that way `bougie service add server` is
            // a self-contained subsystem that doesn't fight a hand-
            // authored ~/.config/bougie/server.toml. The provisioner
            // (`provisioners::bougie_server`) writes hosts to the
            // same path.
            let cfg = paths.service_conf("server", version).join("server.toml");
            let port = endpoint.map_or(7080, |e| e.primary);
            vec![
                "server".into(),
                "run".into(),
                "--config".into(),
                cfg.display().to_string(),
                "--listen".into(),
                format!("127.0.0.1:{port}"),
            ]
        }
        "opensearch" => {
            let data = paths.service_data("opensearch", version).display().to_string();
            let log = paths.service_log("opensearch", version).display().to_string();
            // OpenSearch writes JNA-extracted native libs + assorted
            // temporaries under `OPENSEARCH_TMPDIR`. The sandbox hides
            // /tmp (ProtectSystem::Strict), so pin it under the data
            // dir which is already RW. Created by `pre_start`.
            // Effective ports from endpoint.json (catalog defaults 9200 /
            // 9300 when nothing recorded). Both are relocated when taken,
            // so two search engines — or opensearch beside elasticsearch —
            // coexist.
            let http_port = endpoint.map_or(9200, |e| e.primary);
            let transport_port =
                endpoint.and_then(|e| e.extra_port("transport")).unwrap_or(OPENSEARCH_TRANSPORT_PORT);
            vec![
                format!("-Epath.data={data}"),
                format!("-Epath.logs={log}"),
                // Loopback only — bougie service never bind public
                // addresses (SERVICES.md §6).
                "-Enetwork.host=127.0.0.1".into(),
                format!("-Ehttp.port={http_port}"),
                // Transport port is bound even in single-node mode; pin it
                // to the allocated value so a sibling instance doesn't
                // collide on the default 9300.
                format!("-Etransport.port={transport_port}"),
                // No cluster bootstrap — single-node dev mode skips
                // discovery + initial_cluster_manager_nodes ceremony.
                "-Ediscovery.type=single-node".into(),
            ]
        }
        "mariadb" => {
            let data_path = paths.service_data("mariadb", version);
            let datadir = data_path.display().to_string();
            let sock = paths.service_run("mariadb", version).join("mariadb.sock").display().to_string();
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
            let basedir = store_layout::basedir(paths, entry, version)
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
        "mailpit" => {
            // Loopback-only, like every bougie service (SERVICES.md §6).
            // SMTP is the catalog binding (health-probed); the web UI
            // rides on MAILPIT_HTTP_PORT. Persist caught mail to the
            // service data dir so it survives restarts — the dir is
            // created (and made RW) by the sandbox before spawn, and
            // Mailpit creates the SQLite file + its WAL siblings there.
            let smtp = format!("127.0.0.1:{}", endpoint.map_or(catalog::MAILPIT_SMTP_PORT, |e| e.primary));
            let http = format!(
                "127.0.0.1:{}",
                endpoint
                    .and_then(|e| e.extra_port("http"))
                    .unwrap_or(catalog::MAILPIT_HTTP_PORT)
            );
            let db = paths
                .service_data("mailpit", version)
                .join("mailpit.db")
                .display()
                .to_string();
            vec![
                "--smtp".into(),
                smtp,
                "--listen".into(),
                http,
                "--database".into(),
                db,
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

#[tracing::instrument(skip_all, fields(service = inst.name.as_str(), version = inst.version.as_str()))]
async fn wait_for_health(inst: &Instance, paths: &Paths, child: &mut Child) -> Result<()> {
    let name = inst.name.as_str();
    let version = inst.version.as_str();
    let timeout = health_timeout_for(name);
    let deadline = Instant::now() + timeout;
    loop {
        // Short-circuit on early child exit BEFORE the protocol probe —
        // otherwise a port-collision (opensearch can't bind 9200 because
        // someone else is on it) could look "Running" to us simply
        // because *some* server answers on 9200.
        if let Ok(Some(status)) = child.try_wait() {
            // Give the pipe forwarders a beat to drain the child's
            // last words into the log before quoting it.
            tokio::time::sleep(LOG_DRAIN_GRACE).await;
            return Err(eyre!(
                "service `{name}` exited during startup (status {status}); \
                 check `bougie service logs {name}` for the full log{}",
                startup_log_excerpt(name, version, paths),
            ));
        }
        // Protocol-aware readiness (see `health::probe`): a service is
        // only healthy once it can actually answer, not merely once its
        // port is bound.
        let last_err = match super::health::probe(name, version, paths).await {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if Instant::now() >= deadline {
            return Err(eyre!(
                "service `{name}` did not become healthy within {timeout:?} \
                 (last probe: {last_err:#}); check `bougie service logs {name}`{}",
                startup_log_excerpt(name, version, paths),
            ));
        }
        tokio::time::sleep(HEALTH_POLL).await;
    }
}

/// Pause between noticing a startup exit and quoting the service log,
/// so the stdio forwarder tasks have flushed the child's final output.
const LOG_DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Lines quoted from a failing service's log into the start error.
const STARTUP_LOG_EXCERPT_LINES: usize = 15;
/// Byte cap on the quoted excerpt — a service spewing one enormous
/// line must not balloon the error chain.
const STARTUP_LOG_EXCERPT_BYTES: usize = 4 * 1024;

/// The last few service-log lines, formatted for embedding in a start
/// error (so the *cause* — an EADDRINUSE, a bad config line — travels
/// with the failure into `status`, `bougied.log`, and `bougie
/// diagnose` instead of staying buried on the daemon host). Empty
/// string when there is no log to quote.
fn startup_log_excerpt(name: &str, version: &str, paths: &Paths) -> String {
    let lines = super::logs::tail_lines(&paths.service_log_file(name, version), STARTUP_LOG_EXCERPT_LINES)
        .unwrap_or_default();
    let mut excerpt = String::new();
    for line in &lines {
        let line = line.trim_end_matches(['\n', '\r']);
        if excerpt.len() + line.len() > STARTUP_LOG_EXCERPT_BYTES {
            break;
        }
        excerpt.push_str("\n  ");
        excerpt.push_str(line);
    }
    if excerpt.is_empty() {
        return excerpt;
    }
    format!("; last log lines:{excerpt}")
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

/// Poll a service's process group until it is empty or `budget` elapses.
/// Used when `stop` has no babysit `Child` handle to await (the service
/// was mid-`HealthChecking`): the babysit tears the group down on
/// control-socket EOF, and this lets that graceful `killpg` run before
/// the hard backstop. `test_kill_process_group` is `kill(-pgid, 0)`:
/// `Ok` while members remain, `Err(ESRCH)` once the group is empty.
async fn wait_for_group_drain(pgid: i32, budget: Duration) {
    let Some(pgrp) = rustix::process::Pid::from_raw(pgid) else {
        return;
    };
    let deadline = Instant::now() + budget;
    while rustix::process::test_kill_process_group(pgrp).is_ok() {
        if Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
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

/// Start a service, running its (up-to-90s) health probe *off* the
/// supervisor lock. This is the single entry point for operator starts,
/// boot-time restores, and the reaper's auto-restarts — all of which
/// previously held `Mutex<Supervisor>` across the probe, stalling
/// `status` and the 1s reaper tick. The lock is taken only to spawn the
/// babysit ([`Supervisor::spawn_service`]) and again, briefly, to resolve
/// the outcome ([`Supervisor::finalize_start`]) once the probe returns.
///
/// Returns `Ok(true)` if this call brought the service up, `Ok(false)` if
/// it was already running (or was stopped mid-probe), `Err` if the spawn
/// or the health probe failed.
pub async fn start_service(sup: &Shared, inst: &Instance) -> Result<bool> {
    let mut pending = match sup.lock().await.spawn_service(inst).await? {
        SpawnOutcome::AlreadyRunning => return Ok(false),
        // Move out of the box so the fields below can be moved out by value.
        SpawnOutcome::Spawned(pending) => *pending,
    };
    // Off-lock: the supervisor mutex is free for `status` and the reaper
    // while this probe runs.
    let inst = pending.instance.clone();
    let probe = pending.wait_healthy().await;
    let child = pending.child;
    // Re-lock only to finalize. Bind the guard to a statement so it drops
    // before the `Superseded` arm awaits the child reap.
    let finalize = sup.lock().await.finalize_start(&inst, child, probe);
    match finalize {
        StartFinalize::Started => Ok(true),
        StartFinalize::Failed(e) => Err(e),
        StartFinalize::Superseded(mut child) => {
            // Stopped mid-probe; the babysit got EOF + a group teardown
            // from `stop`. Reap it off-lock so it doesn't linger.
            let _ = child.wait().await;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

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
        let args = render_exec_args(entry, entry.version, &paths, None);
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
        let err = supervisor.binary_path(entry, entry.version).unwrap_err();
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
        let path = supervisor.binary_path(entry, entry.version).unwrap();
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
        let path = supervisor.binary_path(entry, entry.version).unwrap();
        assert!(path.to_string_lossy().contains("redis-8.6.3-abc123"));
    }

    #[test]
    fn snapshot_is_empty_until_instances_are_seeded() {
        // Lazy creation: a fresh supervisor holds no instances, so its
        // snapshot is empty (the pre-lazy version pre-seeded a Stopped slot
        // per catalog entry). Seeding an instance — what the first `up`
        // does — makes it appear as Stopped with its resolved version.
        let mut sup = test_supervisor();
        assert!(sup.snapshot().is_empty(), "no instances until one is upped");
        let redis = seed(&mut sup, "redis");
        seed(&mut sup, "mariadb");
        let snap = sup.snapshot();
        assert!(snap.iter().any(|s| s.name == "redis"
            && s.version == redis.version
            && s.state == ServiceState::Stopped));
        assert!(snap.iter().any(|s| s.name == "mariadb"));
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
        let mut sup = test_supervisor();
        seed(&mut sup, "redis");
        seed(&mut sup, "mariadb");
        let snap = sup.snapshot();
        assert!(!snap.is_empty(), "seeded instances present");
        for s in &snap {
            assert_eq!(s.failure_count, 0, "{}: failure_count should be 0", s.name);
            assert!(s.next_restart_ms.is_none(), "{}: no respawn pending", s.name);
        }
    }

    fn test_supervisor() -> Supervisor {
        let tmp = tempfile::TempDir::new().unwrap();
        // Leak the TempDir so the store path stays valid for the test's
        // lifetime without threading the guard through every assertion.
        let path: std::path::PathBuf = tmp.keep();
        Supervisor::new(Paths::new(path.clone(), path))
    }

    /// Seed a `Stopped` slot for `name`'s default-version instance and
    /// return its instance identity. Lazy creation leaves the map empty, so
    /// a test that pokes `services.get_mut(&id)` (or drives a lifecycle
    /// method that expects an existing slot) seeds first — this stands in
    /// for the first `up` having materialized the instance. Idempotent.
    fn seed(sup: &mut Supervisor, name: &'static str) -> Instance {
        let version = catalog::default_version(name);
        let inst = Instance::new(name, version);
        sup.services
            .entry(inst.id())
            .or_insert_with(|| ManagedService::new(name, version));
        inst
    }

    #[test]
    fn resolve_endpoint_relocates_off_a_squatted_catalog_port() {
        let sup = test_supervisor();
        let entry = catalog::find("rabbitmq").unwrap();
        // Squat rabbitmq's AMQP port. Only a sandbox forbidding loopback
        // binds makes the scenario unbuildable — skip then.
        let _squat = std::net::TcpListener::bind("127.0.0.1:5672").ok();
        if !crate::daemon::ports::port_in_use(5672) {
            return;
        }
        // No hard-fail any more: it relocates and records the new port.
        let ep = sup
            .resolve_endpoint(entry, entry.version)
            .unwrap()
            .expect("a tcp service has an endpoint");
        assert_ne!(ep.primary, 5672, "must relocate off the squatted port");
        assert!(ep.primary > 5672, "scans upward from the default: {}", ep.primary);
        // Persisted so exec args / health / offline consumers agree.
        let back = endpoint::ServiceEndpoint::load(&sup.paths.service_endpoint("rabbitmq", &entry.version))
            .unwrap()
            .unwrap();
        assert_eq!(back, ep);
    }

    #[test]
    fn resolve_endpoint_uses_defaults_when_free_and_is_sticky() {
        let sup = test_supervisor();
        let entry = catalog::find("mailpit").unwrap(); // 1025 SMTP + 8025 http
        if crate::daemon::ports::port_in_use(1025) || crate::daemon::ports::port_in_use(8025) {
            return; // something already holds a default here — skip
        }
        let ep = sup.resolve_endpoint(entry, entry.version).unwrap().unwrap();
        assert_eq!(ep.primary, 1025);
        assert_eq!(ep.extra_port("http"), Some(8025));
        // Sticky: a second resolve reuses the recorded ports verbatim.
        assert_eq!(sup.resolve_endpoint(entry, entry.version).unwrap().unwrap(), ep);
    }

    #[test]
    fn resolve_endpoint_is_none_for_socket_services() {
        let sup = test_supervisor();
        let entry = catalog::find("redis").unwrap();
        assert!(sup.resolve_endpoint(entry, entry.version).unwrap().is_none());
    }

    #[test]
    fn startup_log_excerpt_is_empty_without_a_log() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path: std::path::PathBuf = tmp.keep();
        let paths = Paths::new(path.clone(), path);
        assert_eq!(
            startup_log_excerpt("redis", catalog::default_version("redis"), &paths),
            ""
        );
    }

    #[test]
    fn startup_log_excerpt_quotes_last_lines() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path: std::path::PathBuf = tmp.keep();
        let paths = Paths::new(path.clone(), path);
        let log = paths.service_log_file("redis", crate::daemon::catalog::default_version("redis"));
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let mut contents = String::new();
        for i in 0..40 {
            let _ = writeln!(contents, "line {i}");
        }
        contents.push_str("Address already in use: 127.0.0.1:6379\n");
        std::fs::write(&log, contents).unwrap();
        let excerpt = startup_log_excerpt("redis", catalog::default_version("redis"), &paths);
        assert!(excerpt.starts_with("; last log lines:"), "{excerpt}");
        assert!(excerpt.contains("Address already in use"), "{excerpt}");
        // Only the tail is quoted.
        assert!(!excerpt.contains("line 0\n"), "{excerpt}");
    }

    #[tokio::test]
    async fn startup_exit_error_quotes_the_service_log() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path: std::path::PathBuf = tmp.keep();
        let paths = Paths::new(path.clone(), path);
        let log = paths.service_log_file("redis", crate::daemon::catalog::default_version("redis"));
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "BOOT FAILED: Address already in use\n").unwrap();
        // Stand-in for a babysit whose service died at startup.
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exit 3"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh");
        let inst = Instance::new("redis", catalog::default_version("redis"));
        let err = wait_for_health(&inst, &paths, &mut child).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("exited during startup"), "{msg}");
        assert!(msg.contains("BOOT FAILED: Address already in use"), "{msg}");
        assert!(msg.contains("bougie service logs redis"), "{msg}");
    }

    /// A live child handle to stand in for a babysit in finalize tests.
    /// `sleep` keeps it alive until we explicitly reap it.
    fn spawn_dummy_child() -> Child {
        tokio::process::Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep")
    }

    async fn reap(mut child: Child) {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn finalize_start_running_on_healthy_probe() {
        let mut sup = test_supervisor();
        let redis = seed(&mut sup, "redis");
        sup.services.get_mut(&redis.id()).unwrap().state = ServiceState::HealthChecking;
        let child = spawn_dummy_child();
        let outcome = sup.finalize_start(&redis, child, Ok(()));
        assert!(matches!(outcome, StartFinalize::Started));
        let svc = sup.services.get(&redis.id()).unwrap();
        assert_eq!(svc.state, ServiceState::Running);
        assert!(svc.child.is_some(), "child put back in the map");
        reap(sup.services.get_mut(&redis.id()).unwrap().child.take().unwrap()).await;
    }

    #[tokio::test]
    async fn finalize_start_failed_on_probe_error() {
        let mut sup = test_supervisor();
        let redis = seed(&mut sup, "redis");
        sup.services.get_mut(&redis.id()).unwrap().state = ServiceState::HealthChecking;
        let child = spawn_dummy_child();
        let outcome = sup.finalize_start(&redis, child, Err(eyre::eyre!("port never opened")));
        assert!(matches!(outcome, StartFinalize::Failed(_)));
        let svc = sup.services.get(&redis.id()).unwrap();
        assert_eq!(svc.state, ServiceState::Failed);
        // The (dead/wedged) child is kept so the next check_all tick reaps it.
        assert!(svc.child.is_some());
        reap(sup.services.get_mut(&redis.id()).unwrap().child.take().unwrap()).await;
    }

    #[tokio::test]
    async fn finalize_start_superseded_when_stopped_mid_probe() {
        // The race the off-lock probe newly admits: `stop` runs while the
        // probe is in flight. finalize_start must NOT resurrect the
        // service — it must hand the stale child back instead of marking a
        // stopped service Running.
        let mut sup = test_supervisor();
        let redis = seed(&mut sup, "redis");
        sup.services.get_mut(&redis.id()).unwrap().state = ServiceState::Stopped;
        let child = spawn_dummy_child();
        let outcome = sup.finalize_start(&redis, child, Ok(()));
        let StartFinalize::Superseded(stale) = outcome else {
            panic!("expected Superseded when the service left HealthChecking");
        };
        let svc = sup.services.get(&redis.id()).unwrap();
        assert_eq!(svc.state, ServiceState::Stopped, "honor the newer state");
        assert!(svc.child.is_none(), "do not stash the stale child in the map");
        reap(stale).await;
    }

    #[tokio::test]
    async fn check_all_returns_due_restarts_and_clears_deadline() {
        let mut sup = test_supervisor();
        let redis = seed(&mut sup, "redis");
        {
            let svc = sup.services.get_mut(&redis.id()).unwrap();
            svc.state = ServiceState::Failed;
            // `now` here is <= the `now` check_all samples, so it's due.
            svc.restart_at = Some(Instant::now());
        }
        let due = sup.check_all().await;
        assert!(due.iter().any(|i| i.name == "redis"), "overdue Failed service is due");
        let svc = sup.services.get(&redis.id()).unwrap();
        // Deadline cleared so the next tick won't double-issue the restart
        // that's now in flight off-lock.
        assert!(svc.restart_at.is_none());
        // State stays Failed — the actual respawn happens via start_service.
        assert_eq!(svc.state, ServiceState::Failed);
    }

    #[tokio::test]
    async fn check_all_skips_failed_service_with_future_deadline() {
        let mut sup = test_supervisor();
        // Comfortably in the future (and not a round minute, which a
        // pedantic lint would rather see as `from_mins`).
        let redis = seed(&mut sup, "redis");
        let deadline = Instant::now() + Duration::from_secs(90);
        {
            let svc = sup.services.get_mut(&redis.id()).unwrap();
            svc.state = ServiceState::Failed;
            svc.restart_at = Some(deadline);
        }
        let due = sup.check_all().await;
        assert!(due.is_empty(), "not-yet-due restart is left alone");
        assert_eq!(sup.services.get(&redis.id()).unwrap().restart_at, Some(deadline));
    }

    #[test]
    fn note_restart_failure_arms_backoff_when_idle() {
        let mut sup = test_supervisor();
        let redis = seed(&mut sup, "redis");
        {
            let svc = sup.services.get_mut(&redis.id()).unwrap();
            svc.state = ServiceState::Failed;
            svc.failure_count = 1;
            svc.restart_at = None;
            svc.child = None;
        }
        sup.note_restart_failure(&redis);
        let svc = sup.services.get(&redis.id()).unwrap();
        assert_eq!(svc.failure_count, 2, "spawn-phase failure bumps the count");
        assert!(svc.restart_at.is_some(), "next backoff armed");
    }

    #[test]
    fn note_restart_failure_noops_when_already_rearmed() {
        let mut sup = test_supervisor();
        let redis = seed(&mut sup, "redis");
        let deadline = Instant::now() + Duration::from_secs(99);
        {
            let svc = sup.services.get_mut(&redis.id()).unwrap();
            svc.state = ServiceState::Failed;
            svc.failure_count = 3;
            svc.restart_at = Some(deadline);
        }
        sup.note_restart_failure(&redis);
        let svc = sup.services.get(&redis.id()).unwrap();
        // A probe-phase failure (handled by check_all) or an already-armed
        // deadline must not be double-counted.
        assert_eq!(svc.failure_count, 3);
        assert_eq!(svc.restart_at, Some(deadline));
    }

    // -------------------- continuous health --------------------

    /// Seed a service and put it into a live, probe-able state for the
    /// health tests. Returns the instance identity so callers can address
    /// the same `(name, version)` slot.
    fn mark_running(sup: &mut Supervisor, name: &'static str) -> Instance {
        let inst = seed(sup, name);
        let svc = sup.services.get_mut(&inst.id()).unwrap();
        svc.state = ServiceState::Running;
        svc.started_at = Some(Instant::now());
        svc.next_health_at = Some(Instant::now());
        svc.health_inflight = false;
        svc.health_misses = 0;
        inst
    }

    #[test]
    fn record_health_pass_resets_misses() {
        let mut sup = test_supervisor();
        let redis = mark_running(&mut sup, "redis");
        sup.services.get_mut(&redis.id()).unwrap().health_misses = 2;
        let out = sup.record_health(&redis, true);
        assert_eq!(out, HealthOutcome::Healthy);
        let svc = sup.services.get(&redis.id()).unwrap();
        assert_eq!(svc.health_misses, 0);
        assert_eq!(svc.state, ServiceState::Running);
        assert!(!svc.health_inflight);
        assert!(svc.next_health_at.is_some(), "next probe rescheduled");
    }

    #[test]
    fn record_health_first_miss_marks_unhealthy_but_not_breach() {
        let mut sup = test_supervisor();
        let redis = mark_running(&mut sup, "redis");
        let out = sup.record_health(&redis, false);
        assert_eq!(out, HealthOutcome::Degraded);
        let svc = sup.services.get(&redis.id()).unwrap();
        assert_eq!(svc.health_misses, 1);
        assert_eq!(svc.state, ServiceState::Unhealthy);
    }

    #[test]
    fn record_health_recovers_unhealthy_to_running() {
        let mut sup = test_supervisor();
        let redis = mark_running(&mut sup, "redis");
        {
            let svc = sup.services.get_mut(&redis.id()).unwrap();
            svc.state = ServiceState::Unhealthy;
            svc.health_misses = 2;
        }
        let out = sup.record_health(&redis, true);
        assert_eq!(out, HealthOutcome::Healthy);
        let svc = sup.services.get(&redis.id()).unwrap();
        assert_eq!(svc.state, ServiceState::Running);
        assert_eq!(svc.health_misses, 0);
    }

    #[test]
    fn record_health_breaches_at_threshold() {
        let mut sup = test_supervisor();
        let redis = mark_running(&mut sup, "redis");
        sup.services.get_mut(&redis.id()).unwrap().health_misses = HEALTH_FAILURE_THRESHOLD - 1;
        let out = sup.record_health(&redis, false);
        assert_eq!(out, HealthOutcome::Breach);
        let svc = sup.services.get(&redis.id()).unwrap();
        assert_eq!(svc.health_misses, HEALTH_FAILURE_THRESHOLD);
        assert_eq!(svc.state, ServiceState::Unhealthy);
        // Out of the rotation until fail_unhealthy/restart re-arms it.
        assert!(svc.next_health_at.is_none());
    }

    #[test]
    fn record_health_discards_stale_result_when_not_probeable() {
        // The probe ran off-lock and the service was stopped meanwhile.
        let mut sup = test_supervisor();
        let redis = seed(&mut sup, "redis"); // seeded Stopped
        let out = sup.record_health(&redis, false);
        assert_eq!(out, HealthOutcome::Gone);
        assert_eq!(sup.services.get(&redis.id()).unwrap().state, ServiceState::Stopped);
    }

    #[test]
    fn health_due_picks_running_and_marks_inflight() {
        let mut sup = test_supervisor();
        let redis = mark_running(&mut sup, "redis");
        let due = sup.health_due();
        assert!(due.iter().any(|i| i.name == "redis"));
        assert!(sup.services.get(&redis.id()).unwrap().health_inflight);
        // A second pass must not re-issue while the first is in flight.
        assert!(!sup.health_due().iter().any(|i| i.name == "redis"));
    }

    #[test]
    fn health_due_skips_stopped_and_unprobeable() {
        let mut sup = test_supervisor();
        // Stopped redis: not due.
        // jdk has Binding::None — never probed even if somehow "Running".
        seed(&mut sup, "redis"); // seeded Stopped
        let jdk = seed(&mut sup, "jdk");
        {
            let j = sup.services.get_mut(&jdk.id()).unwrap();
            j.state = ServiceState::Running;
            j.next_health_at = Some(Instant::now());
        }
        let due = sup.health_due();
        assert!(!due.iter().any(|i| i.name == "redis"), "stopped service not probed");
        assert!(!due.iter().any(|i| i.name == "jdk"), "Binding::None never probed");
    }

    #[tokio::test]
    async fn fail_unhealthy_is_noop_unless_unhealthy() {
        let mut sup = test_supervisor();
        let redis = mark_running(&mut sup, "redis"); // Running, not Unhealthy
        sup.fail_unhealthy(&redis).await;
        // Untouched: still Running, no restart scheduled.
        let svc = sup.services.get(&redis.id()).unwrap();
        assert_eq!(svc.state, ServiceState::Running);
        assert!(svc.restart_at.is_none());
    }

    #[tokio::test]
    async fn fail_unhealthy_tears_down_and_schedules_restart() {
        let mut sup = test_supervisor();
        let redis = seed(&mut sup, "redis");
        let child = spawn_dummy_child();
        {
            let svc = sup.services.get_mut(&redis.id()).unwrap();
            svc.state = ServiceState::Unhealthy;
            svc.pid = Some(child.id().unwrap());
            svc.child = Some(child);
            // Short prior run → no failure-count reset; this is failure #3.
            svc.started_at = Some(Instant::now());
            svc.failure_count = 2;
            svc.health_misses = HEALTH_FAILURE_THRESHOLD;
        }
        sup.fail_unhealthy(&redis).await;
        let svc = sup.services.get(&redis.id()).unwrap();
        assert_eq!(svc.state, ServiceState::Failed);
        assert_eq!(svc.failure_count, 3, "breach counts as a fresh failure");
        assert_eq!(svc.health_misses, 0);
        assert!(svc.restart_at.is_some(), "backoff respawn scheduled");
        assert!(svc.child.is_none(), "wedged process torn down");
    }

    #[tokio::test]
    async fn fail_unhealthy_gives_up_past_the_attempt_cap() {
        let mut sup = test_supervisor();
        let redis = seed(&mut sup, "redis");
        let child = spawn_dummy_child();
        {
            let svc = sup.services.get_mut(&redis.id()).unwrap();
            svc.state = ServiceState::Unhealthy;
            svc.pid = Some(child.id().unwrap());
            svc.child = Some(child);
            svc.started_at = Some(Instant::now());
            // Already at the cap → the next failure exceeds it.
            svc.failure_count = MAX_RESTART_ATTEMPTS;
        }
        sup.fail_unhealthy(&redis).await;
        let svc = sup.services.get(&redis.id()).unwrap();
        assert_eq!(svc.state, ServiceState::Failed);
        assert!(
            svc.restart_at.is_none(),
            "past MAX_RESTART_ATTEMPTS the supervisor stops respawning"
        );
    }

    #[test]
    fn snapshot_surfaces_health_misses_and_threshold() {
        let mut sup = test_supervisor();
        let redis_inst = mark_running(&mut sup, "redis");
        {
            let svc = sup.services.get_mut(&redis_inst.id()).unwrap();
            svc.state = ServiceState::Unhealthy;
            svc.health_misses = 2;
        }
        let snap = sup.snapshot();
        let redis = snap.iter().find(|s| s.name == "redis").unwrap();
        assert_eq!(redis.state, ServiceState::Unhealthy);
        assert_eq!(redis.health_misses, 2);
        assert_eq!(redis.health_threshold, HEALTH_FAILURE_THRESHOLD);
    }

    #[tokio::test]
    async fn stop_disarms_failed_service_and_prevents_resurrection() {
        // A crashed service sits in Failed with an armed restart deadline.
        // Taking it down (e.g. the last tenant deprovisioned) must stop it,
        // not no-op — otherwise check_all resurrects it from the deadline.
        let mut sup = test_supervisor();
        let redis = seed(&mut sup, "redis");
        {
            let svc = sup.services.get_mut(&redis.id()).unwrap();
            svc.state = ServiceState::Failed;
            svc.failure_count = 3;
            svc.restart_at = Some(Instant::now()); // due now
            svc.child = None; // crashed-and-already-reaped
        }
        let stopped = sup.stop(&redis).await.unwrap();
        assert!(stopped, "stop() must act on a Failed service");
        let svc = sup.services.get(&redis.id()).unwrap();
        assert_eq!(svc.state, ServiceState::Stopped);
        assert!(svc.restart_at.is_none(), "armed restart deadline cleared");
        assert_eq!(svc.failure_count, 0, "backoff budget reset");

        // The restart pass must not bring it back — its state is Stopped now.
        let due = sup.check_all().await;
        assert!(!due.iter().any(|i| i.name == "redis"), "an explicitly stopped service is not restarted");
    }

    #[tokio::test]
    async fn stop_kills_live_child_parked_under_failed() {
        // A probe timeout leaves the service running while state is Failed
        // (finalize_start stashes the still-live child). Before this fix
        // stop() refused Failed, so the service was unstoppable. It must now
        // take and kill the child.
        let mut sup = test_supervisor();
        let redis = seed(&mut sup, "redis");
        let child = spawn_dummy_child();
        {
            let svc = sup.services.get_mut(&redis.id()).unwrap();
            svc.state = ServiceState::Failed;
            svc.child = Some(child);
            svc.restart_at = None;
        }
        let stopped = sup.stop(&redis).await.unwrap();
        assert!(stopped);
        let svc = sup.services.get(&redis.id()).unwrap();
        assert_eq!(svc.state, ServiceState::Stopped);
        assert!(svc.child.is_none(), "the live child was taken and killed");
    }
}
