//! php-fpm pool lifecycle. Phase 2 surface:
//!
//! - Spawn one pool per `(project, php-version+flavor, variant="normal")`.
//! - Variant conf.d (excluding `debug_only_extensions`) materialized on
//!   spawn; pool `.conf` rendered alongside it.
//! - FCGI_GET_VALUES health probe before the pool is dispatchable.
//! - Stderr captured + line-prefixed with `[fpm:<project>:<variant>]`.
//!
//! Out of phase 2 (deferred to phase 4): idle-out, LRU cap, file-watch
//! reload, restart on PHP-version change.

use eyre::{Result, WrapErr};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::Mutex;

use super::conf_d::{self, PoolConf};
use super::fastcgi;
use super::paths::{create_dir_0700, ServerPaths};
use crate::paths::Paths;
use crate::state::read_project_resolved;

const POOL_READY_TIMEOUT: Duration = Duration::from_secs(2);
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
    pub socket: PathBuf,
    pub php_version: String,
    /// Started-at, for log decoration + the control socket's
    /// `started_ago_ms` field.
    pub started_at: Instant,
    /// OS pid of the php-fpm master. Cached at spawn so we can SIGUSR2
    /// the master from inside an immutable `&Pool` (no need to lock the
    /// child Mutex on the hot reload path).
    pid: u32,
    /// Millis since UNIX_EPOCH when this pool last served a request
    /// (or was first marked dispatchable). The reaper compares this
    /// against `idle_pool_timeout` to decide what to terminate.
    last_served_at: AtomicU64,
    child: Mutex<Child>,
}

impl Pool {
    pub fn socket(&self) -> &Path {
        &self.socket
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

    /// SIGUSR2 the master. php-fpm catches this as "reload": it
    /// rescans `PHP_INI_SCAN_DIR`, respawns workers, but keeps its PID
    /// and listening socket so in-flight requests aren't disrupted.
    pub fn reload(&self) -> Result<()> {
        let pid = rustix::process::Pid::from_raw(
            i32::try_from(self.pid)
                .map_err(|_| eyre::eyre!("pid {} does not fit in i32", self.pid))?,
        )
        .ok_or_else(|| eyre::eyre!("invalid pid {}", self.pid))?;
        rustix::process::kill_process(pid, rustix::process::Signal::USR2)
            .wrap_err_with(|| format!("SIGUSR2 -> pid {}", self.pid))?;
        Ok(())
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
    debug_only_extensions: Vec<String>,
    idle_pool_timeout: Duration,
    max_concurrent_pools: usize,
    pools: Mutex<HashMap<PoolKey, Arc<Pool>>>,
}

impl PoolManager {
    pub fn new(
        bougie_paths: Paths,
        server_paths: ServerPaths,
        debug_only_extensions: Vec<String>,
        idle_pool_timeout: Duration,
        max_concurrent_pools: usize,
    ) -> Self {
        Self {
            bougie_paths,
            server_paths,
            debug_only_extensions,
            idle_pool_timeout,
            max_concurrent_pools,
            pools: Mutex::new(HashMap::new()),
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

        {
            let map = self.pools.lock().await;
            if let Some(pool) = map.get(&key) {
                return Ok(Arc::clone(pool));
            }
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
        let source_confd = project.join(".bougie").join("conf.d");
        for pool in pools {
            let confd_dir = self
                .server_paths
                .pool_confd(&pool.key.project, &pool.key.variant);
            let exclude = match pool.key.variant.as_str() {
                "normal" => self.debug_only_extensions.clone(),
                _ => Vec::new(),
            };
            conf_d::build_variant_confd(&confd_dir, &source_confd, &exclude)?;
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
        let php_fpm = php_install.join("bin").join("php-fpm");
        if !php_fpm.exists() {
            return Err(eyre::eyre!(
                "php-fpm not found at {} — has `bougie sync` been run for this project?",
                php_fpm.display()
            ));
        }

        let project_dir = self.server_paths.project_dir(project);
        create_dir_0700(&project_dir)?;

        let confd_dir = self.server_paths.pool_confd(project, &key.variant);
        let source_confd = project.join(".bougie").join("conf.d");
        let exclude = match key.variant.as_str() {
            "normal" => self.debug_only_extensions.clone(),
            _ => Vec::new(),
        };
        conf_d::build_variant_confd(&confd_dir, &source_confd, &exclude)?;

        let socket = self.server_paths.pool_socket(project, &key.variant);
        // A stale socket from a previous run blocks bind(); php-fpm
        // does cleanup at startup but we belt-and-brace here.
        let _ = std::fs::remove_file(&socket);

        let conf_path = self.server_paths.pool_conf(project, &key.variant);
        let pool_conf = PoolConf { listen_socket: &socket, php_ini_scan_dir: &confd_dir };
        conf_d::write_pool_conf(&conf_path, &pool_conf)?;

        let mut cmd = tokio::process::Command::new(&php_fpm);
        cmd.arg("-y").arg(&conf_path)
            .arg("-p").arg(&project_dir)
            .arg("-F")
            // php-fpm's stderr carries the per-pool log; capture and
            // forward through bougie's logger.
            .stderr(std::process::Stdio::piped())
            // stdout from php-fpm itself is rarely written to; pipe it
            // anyway so the child doesn't block trying to write to a
            // closed fd if there's something to say.
            .stdout(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .wrap_err_with(|| format!("spawning {}", php_fpm.display()))?;

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

        wait_for_socket(&socket).await?;
        // Health probe: SERVER.md §7.6, 2s timeout. Probe failures
        // surface as 502 to the client.
        fastcgi::probe(&socket)
            .await
            .wrap_err_with(|| format!("FastCGI health probe failed for pool {}", key.variant))?;

        let pid = child
            .id()
            .ok_or_else(|| eyre::eyre!("php-fpm child exited before bougie captured its pid"))?;
        Ok(Pool {
            key: key.clone(),
            socket,
            php_version: format!("{}-{}", key.version, key.flavor),
            started_at: Instant::now(),
            pid,
            last_served_at: AtomicU64::new(now_millis()),
            child: Mutex::new(child),
        })
    }
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
/// (php-fpm creates it during startup). The follow-on FCGI_GET_VALUES
/// probe then validates that the responder is actually accepting
/// requests.
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
}
