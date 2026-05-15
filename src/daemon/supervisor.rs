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
use crate::Paths;
use eyre::{eyre, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::sync::Mutex;

/// Default per-service health-probe budget. Redis comes up in
/// <100ms; mariadb cold-starts in ~3-5s; opensearch needs
/// significantly more because the JVM has to JIT-compile + bootstrap
/// the cluster state. `health_timeout_for` overrides this per
/// service for the slow ones.
const HEALTH_TIMEOUT_DEFAULT: Duration = Duration::from_secs(60);
const HEALTH_POLL: Duration = Duration::from_millis(250);

/// Per-service health-probe deadline. JVM-based services (opensearch
/// today, rabbitmq via erlang/JIT later) need a longer window because
/// JIT compilation + cluster bootstrap dominate cold-start time.
fn health_timeout_for(name: &str) -> Duration {
    match name {
        "opensearch" => Duration::from_secs(90),
        _ => HEALTH_TIMEOUT_DEFAULT,
    }
}

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

    /// Resolve the on-disk path of a service's main binary. Thin
    /// wrapper over `store_layout::binary` so the supervisor and the
    /// per-service provisioners agree on where each service lives.
    fn binary_path(&self, entry: &CatalogEntry) -> Result<std::path::PathBuf> {
        store_layout::binary(&self.paths, entry)
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

        let mut cmd = tokio::process::Command::new(&binary);
        cmd.args(&args)
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // SAFETY: `pre_exec` runs in the child after fork and before
        // exec. `sandbox_run::apply_sandbox` is documented for exactly
        // that call site (no allocations after fork, no signal-unsafe
        // code beyond what Landlock / SBPL syscalls themselves use).
        //
        // `policy` is `None` for catalog entries whose sandbox kind
        // can't be implemented on this platform (today: server's
        // `LightHome` on Linux). Skip pre_exec in that case so the
        // child runs with the daemon's own (user-level) privileges.
        if let Some(policy) = policy {
            #[allow(unsafe_code)]
            unsafe {
                cmd.pre_exec(move || {
                    sandbox_run::apply_sandbox(&policy)
                        .map_err(|e| std::io::Error::other(format!("sandbox: {e}")))
                });
            }
        }
        let mut child = cmd.spawn().wrap_err_with(|| {
            format!("spawning {} via {}", entry.name, binary.display())
        })?;
        let pid = child.id();
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

/// Per-service current_dir override. Returns `None` to inherit
/// bougied's CWD. Today only opensearch uses this — its bundled
/// `config/jvm.options` writes the GC log to a relative `logs/`
/// path that the JVM resolves *before* opensearch.yml's `path.logs`
/// is read, so we anchor CWD to the writable data dir so `logs/`
/// resolves under our RW allowlist.
fn render_exec_cwd(entry: &CatalogEntry, paths: &Paths) -> Option<std::path::PathBuf> {
    match entry.name {
        "opensearch" => Some(paths.service_data("opensearch")),
        _ => None,
    }
}

/// Per-service env injected into the child before spawn. Returns an
/// empty map when the service runs with no extras. Pinned to a small
/// list of keys so the table is auditable at a glance.
fn render_exec_env(entry: &CatalogEntry, paths: &Paths) -> Vec<(String, String)> {
    match entry.name {
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
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    tracing::warn!(service, error = %e, "reading from child pipe");
                    return;
                }
            }
        }
    });
}

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
