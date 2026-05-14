//! Process supervisor: state machine + spawn-with-sandbox + health
//! probes + two-phase stop + topological start order.
//!
//! Phase 3 ships only what redis needs to come online end-to-end.
//! Restart policy, log rotation, and the broader catalog provisioner
//! dispatch land in subsequent phases (5–10).

use super::catalog::{self, Binding, CatalogEntry};
use super::sandbox;
use crate::Paths;
use eyre::{eyre, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Child;
use tokio::sync::Mutex;

/// Hard upper bound on how long we wait for a freshly-spawned service
/// to start accepting connections. Redis comes up in <100ms; mariadb
/// can take a few seconds on first run. 60s is generous.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);
const HEALTH_POLL: Duration = Duration::from_millis(250);

/// Default grace window before escalating SIGTERM → SIGKILL. Matches
/// SERVICES.md §5.3.
const STOP_GRACE: Duration = Duration::from_secs(10);

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
#[derive(Debug)]
pub struct ManagedService {
    pub name: &'static str,
    pub state: ServiceState,
    pub child: Option<Child>,
    pub pid: Option<u32>,
    pub started_at: Option<Instant>,
}

impl ManagedService {
    fn new(name: &'static str) -> Self {
        Self { name, state: ServiceState::Stopped, child: None, pid: None, started_at: None }
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
}

#[derive(Debug)]
pub struct Supervisor {
    services: HashMap<&'static str, ManagedService>,
    paths: Paths,
}

impl Supervisor {
    pub fn new(paths: Paths) -> Self {
        let mut services = HashMap::new();
        for entry in catalog::CATALOG {
            services.insert(entry.name, ManagedService::new(entry.name));
        }
        Self { services, paths }
    }

    /// Snapshot every service for the `status` IPC method.
    pub fn snapshot(&self) -> Vec<ServiceStatus> {
        let mut out: Vec<_> = self
            .services
            .values()
            .filter_map(|svc| {
                let entry = catalog::find(svc.name)?;
                Some(ServiceStatus {
                    name: svc.name.to_string(),
                    state: svc.state,
                    pid: svc.pid,
                    uptime_ms: svc
                        .started_at
                        .map(|t| t.elapsed().as_millis() as u64),
                    binding: entry.binding,
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Resolve the on-disk path of a service's main binary, scanning
    /// `\$BOUGIE_HOME/store/` for any directory starting with the
    /// catalog's `tarball` prefix. Phase 3 expects the tarball to be
    /// present already; auto-fetch via `fetch::fetch_blob` lands in a
    /// follow-up.
    fn binary_path(&self, entry: &CatalogEntry) -> Result<PathBuf> {
        if entry.tarball.is_empty() {
            // `server` re-uses the bougie binary itself.
            let exe = std::env::current_exe().wrap_err("locating current bougie binary")?;
            return Ok(exe);
        }
        let store = self.paths.store();
        let prefix = format!("{}-", entry.tarball);
        // Exact-name match (no hash suffix) is also valid — common in
        // tests that lay out a fixture tarball without the deterministic
        // hash flow.
        let exact = store.join(entry.tarball);
        if exact.is_dir() {
            return Ok(exact.join(entry.binary));
        }
        let mut found = None;
        if let Ok(rd) = std::fs::read_dir(&store) {
            for ent in rd.flatten() {
                let name = ent.file_name();
                if name
                    .to_str()
                    .is_some_and(|s| s.starts_with(&prefix))
                {
                    found = Some(ent.path());
                    break;
                }
            }
        }
        let dir = found.ok_or_else(|| {
            eyre!(
                "service `{}`: tarball `{}` not found under {}. \
                 Tarball auto-fetch is not yet wired (Phase 3 follow-up).",
                entry.name,
                entry.tarball,
                store.display(),
            )
        })?;
        Ok(dir.join(entry.binary))
    }

    /// Spawn a service if it isn't already running. Walks
    /// Stopped → Starting → HealthChecking → Running. Returns `true`
    /// if this call brought the service up; `false` if it was already
    /// running.
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

        // All immutable-self work first so we can later take a single
        // mutable borrow without conflicting with these reads.
        let policy = sandbox::build_policy(entry, &self.paths)
            .wrap_err_with(|| format!("compiling sandbox policy for {}", entry.name))?;
        let binary = self.binary_path(entry)?;
        let args = render_exec_args(entry, &self.paths);
        let log_path = self
            .paths
            .service_log(entry.name)
            .join(format!("{}.log", entry.name));
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .wrap_err_with(|| format!("opening log {}", log_path.display()))?;

        let mut cmd = tokio::process::Command::new(&binary);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file.try_clone().wrap_err("dup log fd")?))
            .stderr(Stdio::from(log_file))
            .kill_on_drop(false);
        // SAFETY: `pre_exec` runs in the child after fork and before
        // exec. `sandbox_run::apply_sandbox` is documented for exactly
        // that call site (no allocations after fork, no signal-unsafe
        // code beyond what Landlock / SBPL syscalls themselves use).
        #[allow(unsafe_code)]
        unsafe {
            cmd.pre_exec(move || {
                sandbox_run::apply_sandbox(&policy)
                    .map_err(|e| std::io::Error::other(format!("sandbox: {e}")))
            });
        }
        let child = cmd.spawn().wrap_err_with(|| {
            format!("spawning {} via {}", entry.name, binary.display())
        })?;
        let pid = child.id();

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
        }

        // Health-probe with the lock-equivalent (`&mut self`) still
        // held — the caller is expected to be holding `Mutex<Supervisor>`
        // once, so concurrent starts of different services serialise
        // here. Acceptable for Phase 3 (single-service redis). Phase 5+
        // moves the probe off the lock.
        let probe_paths = self.paths.clone();
        let entry_name = entry.name;
        let binding = entry.binding;
        match wait_for_health(&binding, entry_name, &probe_paths).await {
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
        let (child, pid) = {
            svc.state = ServiceState::Stopping;
            (svc.child.take(), svc.pid)
        };
        // `svc` goes out of scope here; re-borrow below.

        if let Some(mut child) = child {
            stop_child(&mut child, pid).await;
        }

        if let Some(svc) = self.services.get_mut(entry.name) {
            svc.state = ServiceState::Stopped;
            svc.pid = None;
            svc.started_at = None;
        }
        Ok(true)
    }

    /// Reap any service whose child has exited. Called by the 1-second
    /// ticker. Transitions Running → Failed on unexpected exit.
    pub async fn check_all(&mut self) {
        for svc in self.services.values_mut() {
            let Some(child) = svc.child.as_mut() else {
                continue;
            };
            match child.try_wait() {
                Ok(Some(_status)) => {
                    // Child exited. For Phase 3 just transition to
                    // Failed; Phase 5+ adds deadline-driven restart.
                    svc.state = ServiceState::Failed;
                    svc.child = None;
                    svc.pid = None;
                }
                Ok(None) => {} // still running
                Err(e) => {
                    tracing::warn!(service = svc.name, error = %e, "try_wait failed");
                }
            }
        }
    }
}

// -------------------- helpers --------------------

/// Render `exec_args` for a service. Phase 3 only renders redis args
/// because redis is the only Phase-3 provisioner; other services will
/// have their own arg templates in later phases. The fallback is `[]`
/// (run the binary with no args).
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
        _ => Vec::new(),
    }
}

async fn wait_for_health(binding: &Binding, name: &str, paths: &Paths) -> Result<()> {
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    loop {
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
                "service `{name}` did not start accepting connections within {HEALTH_TIMEOUT:?}"
            ));
        }
        tokio::time::sleep(HEALTH_POLL).await;
    }
}

async fn stop_child(child: &mut Child, pid: Option<u32>) {
    if let Some(pid) = pid {
        // SIGTERM via rustix — the bougie codebase already pulls it in.
        if let Some(rpid) = rustix::process::Pid::from_raw(pid as i32) {
            let _ = rustix::process::kill_process(rpid, rustix::process::Signal::TERM);
        }
    }
    // Wait up to the grace window. If still running, SIGKILL.
    match tokio::time::timeout(STOP_GRACE, child.wait()).await {
        Ok(Ok(_)) => {}
        _ => {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

// -------------------- topological sort --------------------

/// Kahn's algorithm over catalog `requires` + `after`. Returns the
/// services in the order they should be started; cycles return an
/// error (catalog tests catch typos already, so a cycle is the only
/// remaining failure mode).
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
}
