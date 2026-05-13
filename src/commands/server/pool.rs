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
use std::sync::Arc;
use std::time::{Duration, Instant};
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

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct PoolKey {
    pub project: PathBuf,
    pub version: String,
    pub flavor: String,
    pub variant: String,
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
    /// Started-at, for log decoration.
    #[allow(dead_code)]
    pub started_at: Instant,
    child: Mutex<Child>,
}

impl Pool {
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn php_version(&self) -> &str {
        &self.php_version
    }

    /// Send SIGTERM to the pool master. Phase 4 will hang the LRU eviction
    /// and idle-out work off this; phase 2 only invokes it during server
    /// shutdown.
    pub async fn terminate(&self) {
        let mut guard = self.child.lock().await;
        let _ = guard.start_kill();
    }
}

#[derive(Debug)]
pub struct PoolManager {
    bougie_paths: Paths,
    server_paths: ServerPaths,
    debug_only_extensions: Vec<String>,
    pools: Mutex<HashMap<PoolKey, Arc<Pool>>>,
}

impl PoolManager {
    pub fn new(
        bougie_paths: Paths,
        server_paths: ServerPaths,
        debug_only_extensions: Vec<String>,
    ) -> Self {
        Self {
            bougie_paths,
            server_paths,
            debug_only_extensions,
            pools: Mutex::new(HashMap::new()),
        }
    }

    /// Return an existing healthy pool or spawn a new one. Phase 2
    /// always passes `variant = "normal"`; phase 3 adds "xdebug".
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

        Ok(Pool {
            key: key.clone(),
            socket,
            php_version: format!("{}-{}", key.version, key.flavor),
            started_at: Instant::now(),
            child: Mutex::new(child),
        })
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
