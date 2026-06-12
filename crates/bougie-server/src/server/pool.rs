//! php-fpm pool lifecycle.
//!
//! - One pool per `(project, php-version+flavor, variant)`. Variant
//!   "normal" handles every request that isn't explicitly tagged for
//!   debugging; "xdebug" is the lazy debug-friendly twin.
//! - Variant conf.d is materialized on spawn by symlinking from the
//!   project's `.bougie/conf.d{,-debug}/` — normal reads only `conf.d/`,
//!   xdebug reads both. The pool `.conf` is rendered alongside it.
//! - First request to the xdebug variant triggers a synchronous
//!   `bougie install xdebug` (via `ensure_debug_extension`) so users
//!   don't have to `bougie ext add xdebug` manually.
//! - `FCGI_GET_VALUES` health probe before the pool is dispatchable.
//! - Stderr captured + line-prefixed with `[fpm:<project>:<variant>]`.

use eyre::{Result, WrapErr};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::Mutex;

#[cfg(unix)]
use super::conf_d::{self, PoolConf};
#[cfg(windows)]
use super::conf_d;
use super::fastcgi::{self, Transport};
use super::paths::{create_dir_0700, ServerPaths};
use bougie_paths::Paths;
use bougie_fs::state::read_project_resolved;
#[cfg(unix)]
use bougie_fs::state::{read_project_resolved_php_path, system_fpm_for_php};

#[cfg(unix)]
const POOL_READY_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(unix)]
const POOL_READY_POLL: Duration = Duration::from_millis(25);
/// How often the idle reaper scans the pool map. The plan calls for
/// ~10s — finer-grained doesn't help when idle timeouts are minutes.
/// Tests can shorten via `BOUGIE_SERVER_REAPER_PERIOD_MS` so the
/// idle-out path doesn't add 10s per test case.
const DEFAULT_REAPER_PERIOD: Duration = Duration::from_secs(10);

fn reaper_period() -> Duration {
    std::env::var("BOUGIE_SERVER_REAPER_PERIOD_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(DEFAULT_REAPER_PERIOD, Duration::from_millis)
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct PoolKey {
    pub project: PathBuf,
    pub version: String,
    pub flavor: String,
    pub variant: String,
}

/// One row in [`PoolManager::status_snapshot`]. Mirrors the JSON
/// wire shape the control socket emits for `status`.
#[derive(Debug, Clone)]
pub struct PoolStatusRow {
    pub key: PoolKey,
    pub pid: u32,
    pub idle_ms: u64,
    pub started_ago_ms: u64,
    pub php_version: String,
}

impl PoolKey {
    pub fn new(project: &Path, version: &str, flavor: &str, variant: &str) -> Self {
        Self {
            project: project.to_path_buf(),
            version: version.to_owned(),
            flavor: flavor.to_owned(),
            variant: variant.to_owned(),
        }
    }
}

#[derive(Debug)]
pub struct Pool {
    pub key: PoolKey,
    /// Where the router dispatches `FastCGI` requests. Unix: a per-pool
    /// `php-fpm` Unix socket. Windows: a per-pool `php-cgi.exe -b`
    /// TCP loopback endpoint.
    transport: Transport,
    pub php_version: String,
    /// Started-at, for log decoration + the control socket's
    /// `started_ago_ms` field.
    pub started_at: Instant,
    /// OS pid of the php-fpm master (unix) or php-cgi.exe worker
    /// (windows). Cached at spawn so we can SIGUSR2 the master from
    /// inside an immutable `&Pool` (no need to lock the child Mutex on
    /// the hot reload path).
    pid: u32,
    /// Millis since `UNIX_EPOCH` when this pool last served a request
    /// (or was first marked dispatchable). The reaper compares this
    /// against `idle_pool_timeout` to decide what to terminate.
    last_served_at: AtomicU64,
    child: Mutex<Child>,
}

impl Pool {
    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    pub fn php_version(&self) -> &str {
        &self.php_version
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Mark the pool as freshly used. Called by the router right after
    /// `get_or_spawn` returns so a pool actively serving long requests
    /// doesn't get reaped underneath the dispatch.
    pub fn touch(&self) {
        self.last_served_at.store(now_millis(), Ordering::Relaxed);
    }

    /// Age relative to wall-clock now. Used by the reaper.
    pub fn idle_for(&self) -> Duration {
        let then = self.last_served_at.load(Ordering::Relaxed);
        let now = now_millis();
        Duration::from_millis(now.saturating_sub(then))
    }

    /// Send SIGTERM to the pool master. Phase 4 hangs the LRU eviction
    /// and idle-out paths off this; shutdown still calls it on every
    /// pool. `kill_on_drop(true)` on the child is the belt; this is
    /// the braces (so we don't wait until the Arc<Pool> drops to start
    /// reaping the process).
    pub async fn terminate(&self) {
        let mut guard = self.child.lock().await;
        let _ = guard.start_kill();
    }

    /// Is the pool still dispatchable? A php-fpm master can die out from
    /// under us — a crash (xdebug segfault), an OOM kill, or an external
    /// SIGKILL — and php-fpm removes its listen socket on exit. The
    /// router caches the `Arc<Pool>`, so without this check it would keep
    /// dispatching into a vanished socket and 502 forever. We check two
    /// things: the master process is still running (also reaps a zombie
    /// via `try_wait`), and — on Unix — its dispatch socket still exists.
    pub async fn is_alive(&self) -> bool {
        {
            let mut guard = self.child.lock().await;
            match guard.try_wait() {
                // Still running.
                Ok(None) => {}
                // Exited (Some) or unpollable (Err) — treat as dead.
                _ => return false,
            }
        }
        #[cfg(unix)]
        if let Transport::UnixSocket(socket) = &self.transport
            && !socket.exists()
        {
            return false;
        }
        true
    }

    /// Tell the pool to pick up a fresh `conf.d/`. Unix: SIGUSR2 the
    /// php-fpm master so it rescans `PHP_INI_SCAN_DIR`, respawns
    /// workers, but keeps its PID and listening socket so in-flight
    /// requests aren't disrupted.
    ///
    /// Windows: php-cgi.exe has no reload signal and Windows has no
    /// SIGUSR2 anyway; surface as unsupported so the watcher reports
    /// the limitation rather than silently no-op'ing. PR 3 will swap
    /// this for kill-and-respawn semantics.
    pub fn reload(&self) -> Result<()> {
        #[cfg(unix)]
        {
            let pid = rustix::process::Pid::from_raw(
                i32::try_from(self.pid)
                    .map_err(|_| eyre::eyre!("pid {} does not fit in i32", self.pid))?,
            )
            .ok_or_else(|| eyre::eyre!("invalid pid {}", self.pid))?;
            rustix::process::kill_process(pid, rustix::process::Signal::USR2)
                .wrap_err_with(|| format!("SIGUSR2 -> pid {}", self.pid))?;
            Ok(())
        }
        #[cfg(windows)]
        {
            Err(eyre::eyre!(
                "pool reload is not yet implemented on Windows — restart `bougie server` to pick up conf.d changes"
            ))
        }
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[derive(Debug)]
pub struct PoolManager {
    bougie_paths: Paths,
    server_paths: ServerPaths,
    idle_pool_timeout: Duration,
    max_concurrent_pools: usize,
    pools: Mutex<HashMap<PoolKey, Arc<Pool>>>,
}

impl PoolManager {
    pub fn new(
        bougie_paths: Paths,
        server_paths: ServerPaths,
        idle_pool_timeout: Duration,
        max_concurrent_pools: usize,
    ) -> Self {
        Self {
            bougie_paths,
            server_paths,
            idle_pool_timeout,
            max_concurrent_pools,
            pools: Mutex::new(HashMap::new()),
        }
    }

    /// Source `.bougie/conf.d*` dirs to merge into a variant's
    /// `<variant>.confd/`. Normal pool reads only `conf.d/`; xdebug
    /// pool also reads `conf.d-debug/` (where xdebug.ini lives). Order
    /// matters: the regular dir comes first so a primary entry would
    /// shadow a stale duplicate in the debug overlay.
    fn variant_source_dirs(variant: &str, project: &Path) -> Vec<PathBuf> {
        let regular = bougie_installer::conf_d::project_confd_dir(project);
        let local = bougie_installer::conf_d::project_confd_local_dir(project);
        match variant {
            "xdebug" => vec![regular, local, bougie_installer::conf_d::project_confd_debug_dir(project)],
            _ => vec![regular, local],
        }
    }

    pub fn idle_pool_timeout(&self) -> Duration {
        self.idle_pool_timeout
    }

    /// Return an existing healthy pool or spawn a new one. Enforces
    /// the LRU cap: if the map already holds `max_concurrent_pools`
    /// entries, the oldest-idle pool is evicted before the new spawn
    /// (SERVER.md §7.2 "Concurrency cap").
    pub async fn get_or_spawn(&self, project: &Path, variant: &str) -> Result<Arc<Pool>> {
        let (version, flavor) = read_project_resolved(project).wrap_err_with(|| {
            format!(
                "reading .bougie/state/resolved in {}",
                project.display(),
            )
        })?;
        let key = PoolKey::new(project, &version, &flavor, variant);

        let cached = {
            let map = self.pools.lock().await;
            map.get(&key).map(Arc::clone)
        };
        if let Some(pool) = cached {
            if pool.is_alive().await {
                return Ok(pool);
            }
            // The cached master died. Evict it (guarding against a
            // concurrent respawn that already replaced the entry) and
            // fall through to spawn a fresh one — this is what makes the
            // server self-heal after a crashed worker instead of wedging
            // until a full `bougie services restart server`.
            self.evict(&pool).await;
            eprintln!(
                "[pool_dead] project={} variant={} pid={} reason=respawn",
                pool.key.project.display(),
                pool.key.variant,
                pool.pid(),
            );
        }

        // Spawn outside the map lock — php-fpm takes ~tens of ms to
        // boot and we don't want every concurrent request blocking on
        // a sister-pool's spawn.
        let pool = Arc::new(self.spawn(key.clone()).await?);

        let mut map = self.pools.lock().await;
        // Re-check: another task may have raced us and won.
        if let Some(existing) = map.get(&key) {
            // Drop our spawn; the winner stays.
            pool.terminate().await;
            return Ok(Arc::clone(existing));
        }
        // Enforce the concurrency cap *before* inserting. The evicted
        // pool's last_served_at is older than the brand-new one's, so
        // even if the new pool would land below the cutoff a moment
        // later, we never end up over capacity in the steady state.
        if map.len() >= self.max_concurrent_pools {
            evict_lru(&mut map);
        }
        map.insert(key, Arc::clone(&pool));
        Ok(pool)
    }

    /// Kill every pool. Called on server shutdown after the drain.
    pub async fn shutdown(&self) {
        let pools: Vec<Arc<Pool>> = {
            let mut map = self.pools.lock().await;
            map.drain().map(|(_, v)| v).collect()
        };
        for p in pools {
            p.terminate().await;
        }
    }

    /// Walk every active pool whose project matches `project` and
    /// re-issue `build_variant_confd` + SIGUSR2 to the master.
    ///
    /// On file-watch events for `<project>/.bougie/conf.d/`: this is
    /// what swaps in a freshly-installed extension without killing the
    /// master. In-flight requests finish on the old workers; new ones
    /// see the new conf.d.
    pub async fn reload_project(&self, project: &Path) -> Result<usize> {
        let pools = self.snapshot_for_project(project).await;
        let mut reloaded = 0usize;
        for pool in pools {
            let confd_dir = self
                .server_paths
                .pool_confd(&pool.key.project, &pool.key.variant);
            let sources = Self::variant_source_dirs(&pool.key.variant, project);
            let source_refs: Vec<&Path> = sources.iter().map(PathBuf::as_path).collect();
            conf_d::build_variant_confd(&confd_dir, &source_refs)?;
            pool.reload()?;
            reloaded += 1;
        }
        Ok(reloaded)
    }

    /// Drop every active pool whose project matches `project`. The
    /// next request lazily respawns against whatever
    /// `.bougie/state/resolved` says now — so a PHP-version change
    /// rolls in transparently.
    pub async fn restart_project(&self, project: &Path) -> usize {
        let keys_to_drop: Vec<PoolKey> = {
            let map = self.pools.lock().await;
            map.iter()
                .filter(|(k, _)| k.project == project)
                .map(|(k, _)| k.clone())
                .collect()
        };
        let mut count = 0usize;
        for key in keys_to_drop {
            if let Some(pool) = self.pools.lock().await.remove(&key) {
                pool.terminate().await;
                count += 1;
            }
        }
        count
    }

    /// Remove `pool` from the live map iff it's still the cached entry
    /// for its key, then terminate it. Used to self-heal: the fast path
    /// drops a master that died between requests, and the router drops a
    /// master that a dispatch just discovered is gone. The `Arc::ptr_eq`
    /// guard makes this a no-op if another task already respawned and
    /// re-inserted a fresh pool under the same key.
    pub async fn evict(&self, pool: &Arc<Pool>) {
        {
            let mut map = self.pools.lock().await;
            match map.get(&pool.key) {
                Some(existing) if Arc::ptr_eq(existing, pool) => {
                    map.remove(&pool.key);
                }
                _ => return,
            }
        }
        pool.terminate().await;
    }

    /// Periodic scan: SIGTERM any pool whose `idle_for() > idle_pool_timeout`.
    /// Spawned by `start_idle_reaper`; runs until cancelled at shutdown.
    pub async fn reap_idle(&self) -> usize {
        let mut victims: Vec<(PoolKey, Arc<Pool>)> = Vec::new();
        {
            let map = self.pools.lock().await;
            for (k, p) in map.iter() {
                if p.idle_for() >= self.idle_pool_timeout {
                    victims.push((k.clone(), Arc::clone(p)));
                }
            }
        }
        if victims.is_empty() {
            return 0;
        }
        let mut map = self.pools.lock().await;
        for (k, pool) in &victims {
            map.remove(k);
            eprintln!(
                "[pool_idle_out] project={} variant={} pid={} idle={:?}",
                pool.key.project.display(),
                pool.key.variant,
                pool.pid(),
                pool.idle_for(),
            );
        }
        drop(map);
        for (_, pool) in &victims {
            pool.terminate().await;
        }
        victims.len()
    }

    async fn snapshot_for_project(&self, project: &Path) -> Vec<Arc<Pool>> {
        let map = self.pools.lock().await;
        map.iter()
            .filter(|(k, _)| k.project == project)
            .map(|(_, v)| Arc::clone(v))
            .collect()
    }

    /// Live PID list — handy for tests + the phase-6 control socket.
    pub async fn pids(&self) -> Vec<(PoolKey, u32)> {
        let map = self.pools.lock().await;
        map.iter().map(|(k, p)| (k.clone(), p.pid())).collect()
    }

    /// Richer snapshot: pid + idle age + started-ago per pool, for the
    /// control socket's `status` response. `started_ago_ms` is wall-
    /// clock-free (uses tokio's Instant), so it survives system clock
    /// jumps without going negative.
    pub async fn status_snapshot(&self) -> Vec<PoolStatusRow> {
        let map = self.pools.lock().await;
        let now = Instant::now();
        map.iter()
            .map(|(k, p)| PoolStatusRow {
                key: k.clone(),
                pid: p.pid(),
                idle_ms: u64::try_from(p.idle_for().as_millis()).unwrap_or(u64::MAX),
                started_ago_ms: u64::try_from(now.saturating_duration_since(p.started_at).as_millis())
                    .unwrap_or(u64::MAX),
                php_version: p.php_version.clone(),
            })
            .collect()
    }

    async fn spawn(&self, key: PoolKey) -> Result<Pool> {
        let project = &key.project;
        let php_install = self
            .bougie_paths
            .installs()
            .join(format!("{}-{}", key.version, key.flavor));

        // A project pinned to a **system** (Homebrew/distro) PHP records
        // its interpreter in `resolved-php-path`; managed projects don't.
        // System PHP keeps its own php.ini + extensions, so bougie spawns
        // the system php-fpm and skips conf.d injection — its ABI-foreign
        // `.so` fragments can't dlopen onto a foreign build. Unix-only:
        // php-fpm doesn't exist on Windows, and `bougie sync` never
        // selects a system PHP there.
        #[cfg(unix)]
        let system_php = read_project_resolved_php_path(project);
        #[cfg(not(unix))]
        let system_php: Option<PathBuf> = None;

        // Per-platform PHP runtime. Unix uses `php-fpm` (master+workers,
        // unix socket); Windows uses `php-cgi.exe -b 127.0.0.1:<port>`
        // because php-fpm is not built for Windows.
        #[cfg(unix)]
        let binary = match &system_php {
            Some(php) => system_fpm_for_php(php).ok_or_else(|| {
                eyre::eyre!(
                    "the dev server needs php-fpm, but the system PHP at {} has \
                     none alongside it (looked in its bin/ and ../sbin/). Install \
                     your platform's php-fpm package, or switch to a bougie-managed \
                     PHP with `bougie php pin <version>` then `bougie sync`.",
                    php.display()
                )
            })?,
            None => php_install.join("bin").join("php-fpm"),
        };
        #[cfg(windows)]
        let binary = php_install.join("bin").join("php-cgi.exe");

        // For a managed install the binary may simply be missing (never
        // synced); the system branch already verified existence above.
        if system_php.is_none() && !binary.exists() {
            let label = binary
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("php runtime");
            return Err(eyre::eyre!(
                "{label} not found at {} — has `bougie sync` been run for this project?",
                binary.display()
            ));
        }

        // bougie's xdebug build is ABI-matched to its own interpreter, so
        // the xdebug overlay can't load onto a foreign system PHP. Fail
        // with a pointer to a managed PHP rather than spawning a pool that
        // would just ignore the (un-injected) overlay.
        if system_php.is_some() && key.variant == "xdebug" {
            return Err(eyre::eyre!(
                "the xdebug overlay needs a bougie-managed PHP — bougie's xdebug \
                 build is ABI-matched to its own interpreter and can't load onto a \
                 system PHP. Switch with `bougie php pin <version>` then `bougie sync`."
            ));
        }

        // First request to the xdebug variant is the trigger for
        // installing xdebug into the project. We keep it out of
        // `bougie sync` deliberately: xdebug-as-a-project-dependency
        // would force every other dev on the team to install it. The
        // server, in contrast, only loads xdebug when this pool is
        // routed to — so its install can be lazy + server-private.
        if key.variant == "xdebug" {
            ensure_debug_extension(project, "xdebug", &self.bougie_paths).await?;
        }

        let project_dir = self.server_paths.project_dir(project);
        create_dir_0700(&project_dir)?;

        // System PHP loads its own conf.d; bougie injects none. Managed
        // installs get the per-variant conf.d merged from the project's
        // `.bougie/conf.d{,-debug}/`.
        let inject_confd = system_php.is_none();
        let confd_dir = self.server_paths.pool_confd(project, &key.variant);
        if inject_confd {
            let sources = Self::variant_source_dirs(&key.variant, project);
            let source_refs: Vec<&Path> = sources.iter().map(PathBuf::as_path).collect();
            conf_d::build_variant_confd(&confd_dir, &source_refs)?;
        }

        let (transport, mut child) = spawn_runtime(
            &binary,
            &project_dir,
            &confd_dir,
            inject_confd,
            &self.server_paths,
            project,
            &key.variant,
        )
        .await?;

        // Capture stderr in a side task, line-prefix with
        // `[fpm:<project>:<variant>]`. Drains until the child exits.
        if let Some(stderr) = child.stderr.take() {
            let prefix = format!(
                "[fpm:{}:{}]",
                project_label(project),
                key.variant,
            );
            tokio::spawn(forward_lines(stderr, prefix));
        }
        if let Some(stdout) = child.stdout.take() {
            let prefix = format!(
                "[fpm:{}:{}:stdout]",
                project_label(project),
                key.variant,
            );
            tokio::spawn(forward_lines(stdout, prefix));
        }

        // Health probe: SERVER.md §7.6, 2s timeout. Probe failures
        // surface as 502 to the client.
        fastcgi::probe(&transport)
            .await
            .wrap_err_with(|| format!("FastCGI health probe failed for pool {}", key.variant))?;

        let pid = child
            .id()
            .ok_or_else(|| eyre::eyre!("php worker exited before bougie captured its pid"))?;
        Ok(Pool {
            key: key.clone(),
            transport,
            php_version: format!("{}-{}", key.version, key.flavor),
            started_at: Instant::now(),
            pid,
            last_served_at: AtomicU64::new(now_millis()),
            child: Mutex::new(child),
        })
    }
}

/// Platform-specific runtime spawn. Unix: write a php-fpm pool conf,
/// launch php-fpm, wait for the unix socket. Windows: pick a random
/// port in the dynamic range, launch php-cgi.exe with `-b 127.0.0.1:<port>`,
/// retry on bind collision (php-cgi.exe rejects port 0, so we can't ask
/// the kernel to pick one). The probe in the caller is the real
/// "dispatchable" signal in both cases.
#[cfg(unix)]
async fn spawn_runtime(
    binary: &Path,
    project_dir: &Path,
    confd_dir: &Path,
    inject_confd: bool,
    server_paths: &ServerPaths,
    project: &Path,
    variant: &str,
) -> Result<(Transport, Child)> {
    let socket = server_paths.pool_socket(project, variant);
    // A stale socket from a previous run blocks bind(); php-fpm
    // does cleanup at startup but we belt-and-brace here.
    let _ = std::fs::remove_file(&socket);

    let conf_path = server_paths.pool_conf(project, variant);
    // System PHP (`inject_confd == false`): no bougie scan dir, so the
    // interpreter keeps its own compiled-in conf.d + extensions.
    let scan_dir = inject_confd.then_some(confd_dir);
    let pool_conf = PoolConf { listen_socket: &socket, php_ini_scan_dir: scan_dir };
    conf_d::write_pool_conf(&conf_path, &pool_conf)?;

    let mut cmd = tokio::process::Command::new(binary);
    cmd.arg("-y").arg(&conf_path)
        .arg("-p").arg(project_dir)
        .arg("-F");
    // PHP_INI_SCAN_DIR must be set on php-fpm's *own* process
    // env: the master parses INI files at startup, before any
    // worker is forked. The pool conf's `env[PHP_INI_SCAN_DIR]`
    // only reaches workers (it lands in $_ENV/$_SERVER for the
    // PHP script) and so doesn't influence which fragments
    // get loaded. Without this, the merged `xdebug.confd/`
    // (including the xdebug.ini symlink) is scanned but
    // overridden by php-fpm's compiled-in default. Skipped for a
    // system PHP so it loads its own conf.d (Homebrew/distro), not
    // bougie's ABI-foreign fragments.
    if let Some(dir) = scan_dir {
        cmd.env("PHP_INI_SCAN_DIR", dir);
    }
    cmd.stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .wrap_err_with(|| format!("spawning {}", binary.display()))?;

    wait_for_socket(&socket).await?;
    Ok((Transport::UnixSocket(socket), child))
}

#[cfg(windows)]
async fn spawn_runtime(
    binary: &Path,
    project_dir: &Path,
    confd_dir: &Path,
    // Windows never selects a system PHP (php-fpm is Unix-only and sync
    // won't pick one here), so bougie always injects its conf.d.
    _inject_confd: bool,
    _server_paths: &ServerPaths,
    _project: &Path,
    _variant: &str,
) -> Result<(Transport, Child)> {
    use rand::Rng;
    use tokio::io::AsyncReadExt;
    const MAX_ATTEMPTS: usize = 20;
    /// How long we wait after spawning before deciding the bind
    /// succeeded. If the child exits within this window we treat it as
    /// a bind failure (or other startup error) and classify via stderr.
    const READY_WAIT: Duration = Duration::from_millis(200);

    let mut last_err: Option<eyre::Report> = None;
    for _ in 0..MAX_ATTEMPTS {
        // Dynamic port range per Windows convention (also the BSD/IANA
        // ephemeral range). 16384 candidates — collisions are vanishingly
        // unlikely in practice for a dev tool.
        let port: u16 = rand::thread_rng().gen_range(49152..=65535);
        let mut cmd = tokio::process::Command::new(binary);
        cmd.arg("-b")
            .arg(format!("127.0.0.1:{port}"))
            .env("PHP_INI_SCAN_DIR", confd_dir)
            .current_dir(project_dir)
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .wrap_err_with(|| format!("spawning {}", binary.display()))?;
        tokio::time::sleep(READY_WAIT).await;
        match child.try_wait() {
            Ok(None) => {
                // Still running — assume bound. The caller's FastCGI
                // probe is the real "is it accepting requests" signal.
                let addr = format!("127.0.0.1:{port}")
                    .parse::<std::net::SocketAddr>()
                    .expect("constructed loopback addr is valid");
                return Ok((Transport::Tcp(addr), child));
            }
            Ok(Some(status)) => {
                let mut buf = Vec::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_end(&mut buf).await;
                }
                let msg = String::from_utf8_lossy(&buf).into_owned();
                // php-cgi.exe's bind-failure message — the user's
                // explicit retry signal. Anything else is treated as
                // a non-retryable startup error so we don't silently
                // loop on, e.g., a missing PHP extension or bad ini.
                if msg.contains("Couldn't create FastCGI listen socket") {
                    last_err = Some(eyre::eyre!(
                        "port {port} unavailable: {}",
                        msg.trim()
                    ));
                    continue;
                }
                return Err(eyre::eyre!(
                    "php-cgi.exe exited with status {status:?}: {}",
                    msg.trim(),
                ));
            }
            Err(e) => return Err(e).wrap_err("polling php-cgi.exe"),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        eyre::eyre!(
            "could not bind a loopback port for php-cgi.exe after {MAX_ATTEMPTS} attempts"
        )
    }))
}

/// Spawn the periodic idle-out reaper. Returns a `JoinHandle` the
/// server can `abort()` at shutdown. Runs `reap_idle` every
/// `BOUGIE_SERVER_REAPER_PERIOD_MS` (or [`DEFAULT_REAPER_PERIOD`] when
/// unset). Cheap when the pool map is empty.
pub fn start_idle_reaper(manager: Arc<PoolManager>) -> tokio::task::JoinHandle<()> {
    let period = reaper_period();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        // First tick fires immediately; skip it so we don't reap a
        // pool that just barely missed the timeout on the boundary.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let _ = manager.reap_idle().await;
        }
    })
}

/// Evict the LRU pool from `map`. Picks the entry with the oldest
/// `last_served_at`. Caller must hold the map lock. Emits an
/// `[pool_evicted]` log line; the actual SIGTERM happens after the
/// caller releases the lock so we don't sit on it during the
/// async `terminate()`.
fn evict_lru(map: &mut HashMap<PoolKey, Arc<Pool>>) {
    let Some(oldest_key) = map
        .iter()
        .min_by_key(|(_, p)| p.last_served_at.load(Ordering::Relaxed))
        .map(|(k, _)| k.clone())
    else {
        return;
    };
    if let Some(pool) = map.remove(&oldest_key) {
        eprintln!(
            "[pool_evicted] project={} variant={} pid={} reason=lru-cap",
            pool.key.project.display(),
            pool.key.variant,
            pool.pid(),
        );
        // Fire-and-forget the terminate; the kill_on_drop on the
        // child catches the slow case if the SIGTERM dispatch races.
        let pool_clone = pool.clone();
        tokio::spawn(async move {
            pool_clone.terminate().await;
        });
    }
}

/// Wait up to [`POOL_READY_TIMEOUT`] for `socket` to appear on disk
/// (php-fpm creates it during startup). The follow-on `FCGI_GET_VALUES`
/// probe then validates that the responder is actually accepting
/// requests.
#[cfg(unix)]
async fn wait_for_socket(socket: &Path) -> Result<()> {
    let deadline = Instant::now() + POOL_READY_TIMEOUT;
    loop {
        if socket.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(eyre::eyre!(
                "php-fpm pool socket {} didn't appear within {:?}",
                socket.display(),
                POOL_READY_TIMEOUT
            ));
        }
        tokio::time::sleep(POOL_READY_POLL).await;
    }
}

async fn forward_lines<R>(stream: R, prefix: String)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        eprintln!("{prefix} {line}");
    }
}

/// Ensure a debug-only extension is installed and the server's
/// xdebug pool can find a fragment for it. Idempotent: if a fragment
/// for `name` already exists in *either* `.bougie/conf.d/` (because
/// the user ran `bougie ext add xdebug`) or `.bougie/conf.d-debug/`
/// (because a previous request already triggered this path), this is
/// a no-op. Otherwise the .so is fetched/located in the store and a
/// fragment is written to `.bougie/conf.d-debug/` — the server's
/// private overlay, invisible to `bougie run` and the normal pool.
///
/// The install side uses [`bougie_installer::install::install_extension`], which
/// is blocking (uses `reqwest::blocking`), so we hand it to
/// `spawn_blocking` to keep the tokio runtime responsive while a
/// possibly-multi-MB download runs.
async fn ensure_debug_extension(
    project: &Path,
    name: &str,
    bougie_paths: &Paths,
) -> Result<()> {
    if bougie_installer::conf_d::fragment_present_anywhere(project, name) {
        return Ok(());
    }
    let project = project.to_path_buf();
    let project_for_log = project.clone();
    let bougie_paths = bougie_paths.clone();
    let name_owned = name.to_owned();
    let name_for_log = name_owned.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let (php_minor, flavor) =
            bougie_installer::install::resolved_php_for_ext_install(&project)?;
        let installed = bougie_installer::install::install_extension(
            &bougie_paths,
            &name_owned,
            None,
            php_minor,
            flavor,
            bougie_resolver::ResolveOptions::default(),
        )?;
        eprintln!(
            "bougie server: enabling {name_owned} for {} (first xdebug request{})",
            project.display(),
            if installed.already_present { "" } else { "; downloaded" },
        );
        bougie_installer::conf_d::write_debug_overlay_fragment(
            &project,
            &installed.name,
            &installed.so_path,
            installed.load,
        )?;
        Ok(())
    })
    .await
    .map_err(|e| eyre::eyre!("join error enabling {name_for_log} in {}: {e}", project_for_log.display()))??;
    Ok(())
}

/// Short label for a project to embed in `[fpm:<label>:<variant>]`.
/// Trailing path component is the typical project name; falls back to
/// the full path string when the basename is empty (root, weird mount
/// points).
fn project_label(project: &Path) -> String {
    project
        .file_name()
        .and_then(|s| s.to_str())
        .map_or_else(|| project.display().to_string(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_key_hashes_distinct_variants_separately() {
        let mut m: HashMap<PoolKey, u32> = HashMap::new();
        let a = PoolKey::new(Path::new("/p"), "8.3.12", "nts", "normal");
        let b = PoolKey::new(Path::new("/p"), "8.3.12", "nts", "xdebug");
        m.insert(a.clone(), 1);
        m.insert(b.clone(), 2);
        assert_eq!(m.get(&a), Some(&1));
        assert_eq!(m.get(&b), Some(&2));
    }

    #[test]
    fn project_label_uses_basename() {
        assert_eq!(project_label(Path::new("/home/jelle/projects/myapp")), "myapp");
        assert_eq!(project_label(Path::new("/")), "/");
    }

    // Build a Pool wrapping an arbitrary child + socket path so the
    // liveness logic can be tested without a real php-fpm master.
    #[cfg(unix)]
    fn test_pool(child: Child, socket: PathBuf) -> Pool {
        let pid = child.id().expect("child has a pid");
        Pool {
            key: PoolKey::new(Path::new("/p"), "8.3.12", "nts", "xdebug"),
            transport: Transport::UnixSocket(socket),
            php_version: "8.3.12-nts".to_owned(),
            started_at: Instant::now(),
            pid,
            last_served_at: AtomicU64::new(now_millis()),
            child: Mutex::new(child),
        }
    }

    #[cfg(unix)]
    fn spawn_sleeper() -> Child {
        tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep")
    }

    // A live master with a present socket is dispatchable; deleting the
    // socket (php-fpm removes it on exit) flips the pool to not-alive
    // even while the process lingers.
    #[cfg(unix)]
    #[tokio::test]
    async fn is_alive_tracks_socket_presence() {
        let dir = std::env::temp_dir().join(format!("bougie-pool-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("xdebug.sock");
        std::fs::write(&socket, b"").unwrap();

        let pool = test_pool(spawn_sleeper(), socket.clone());
        assert!(pool.is_alive().await, "live process + present socket => alive");

        std::fs::remove_file(&socket).unwrap();
        assert!(!pool.is_alive().await, "missing socket => not alive");

        pool.terminate().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A dead master is not dispatchable even if a stale socket file
    // happens to still be on disk.
    #[cfg(unix)]
    #[tokio::test]
    async fn is_alive_false_when_master_dead() {
        let dir = std::env::temp_dir().join(format!("bougie-pool-dead-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("xdebug.sock");
        std::fs::write(&socket, b"").unwrap();

        let pool = test_pool(spawn_sleeper(), socket);
        assert!(pool.is_alive().await);

        pool.terminate().await;
        // SIGKILL is async; poll until try_wait observes the exit.
        let mut dead = false;
        for _ in 0..200 {
            if !pool.is_alive().await {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(dead, "pool reports not-alive after the master is killed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
