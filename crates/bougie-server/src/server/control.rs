//! Control socket: phase 6 of `SERVER_PLAN.md`. Lets `bougie server list`
//! query a running server for live pool state.
//!
//! Wire format is one JSON object per request and one JSON object per
//! response, both terminated by `\n`. No framing, no auth — the
//! listener is bound to a per-user surface (Unix socket mode 0600, or
//! a Windows named pipe under `\\.\pipe\bougie-server-<hash>` ACL'd
//! to the current user by Windows defaults).
//!
//! Unix listens on a path under `$XDG_RUNTIME_DIR/bougie/server/`;
//! Windows listens on a named pipe whose name is written to a
//! discovery file at `<runtime_root>/control.pipe` so the list client
//! can find it (named pipes have no filesystem rendezvous of their own).
//!
//! ```jsonc
//! // request
//! {"v": 1, "method": "status"}
//! {"v": 1, "method": "reload", "project": "/abs/path"}
//!
//! // response
//! {"schema_version": 1, "ok": true, "pools": [...], "hosts": [...]}
//! {"schema_version": 1, "ok": true, "reloaded_variants": 2}
//! {"schema_version": 1, "ok": false, "error": "..."}
//! ```

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

use super::router::AppState;

/// Max bytes a client may send before we close the connection. The
/// `reload` method is the heaviest user and its payload is a path —
/// 4 KB is plenty even for unusually-deep project locations.
const MAX_REQUEST_BYTES: usize = 4096;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum Request {
    Status,
    Reload { project: PathBuf },
    /// Re-read `server.toml` from disk and rebuild the in-memory
    /// hostname → host map. Called by `bougied` after it mutates
    /// `server.toml` to provision or de-provision a project's
    /// `[[host]]` block (Phase 8). In-flight requests continue
    /// against the old map until the swap completes; new requests
    /// see the new map.
    ReloadConfig,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub schema_version: u32,
    pub ok: bool,
    pub listen_port: u16,
    pub hosts: Vec<String>,
    pub pools: Vec<PoolRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PoolRow {
    pub project: PathBuf,
    pub variant: String,
    pub php_version: String,
    pub pid: u32,
    pub idle_ms: u64,
    pub started_ago_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReloadResponse {
    pub schema_version: u32,
    pub ok: bool,
    pub reloaded_variants: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReloadConfigResponse {
    pub schema_version: u32,
    pub ok: bool,
    pub hosts: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorResponse {
    pub schema_version: u32,
    pub ok: bool,
    pub error: String,
}

impl ErrorResponse {
    fn new(error: impl Into<String>) -> Self {
        Self { schema_version: 1, ok: false, error: error.into() }
    }
}

/// Start the control listener. The `path` argument is platform-typed:
/// on Unix it's the unix-socket path to bind; on Windows it's the path
/// to a discovery file under `runtime_root` where the named-pipe name
/// is recorded for clients to find.
#[cfg(unix)]
pub fn start(state: Arc<AppState>, socket_path: PathBuf) -> Result<ControlHandle> {
    if let Some(parent) = socket_path.parent() {
        super::paths::create_dir_0700(parent)?;
    }
    // Stale socket from a previous run blocks bind(); the kernel
    // doesn't auto-clean unix sockets the way it does abstract ones.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .wrap_err_with(|| format!("binding {}", socket_path.display()))?;
    set_socket_mode(&socket_path, 0o600)?;

    let cleanup_path = socket_path.clone();
    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, state).await {
                            eprintln!("bougie control: connection error: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    eprintln!("bougie control: accept failed: {e}");
                    // A persistent accept failure is rare; back off
                    // briefly so we don't pin a CPU on EAGAIN.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    });

    Ok(ControlHandle { task, cleanup_path })
}

#[cfg(windows)]
pub fn start(state: Arc<AppState>, discovery_path: PathBuf) -> Result<ControlHandle> {
    if let Some(parent) = discovery_path.parent() {
        super::paths::create_dir_0700(parent)?;
    }
    let pipe_name = windows_pipe_name(&discovery_path);

    // Validate the pipe name by creating the first instance up front.
    // Failure here usually means a stale bougie server still owns the
    // name; surface the error before tokio::spawn so the user sees it.
    let first = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_name)
        .wrap_err_with(|| format!("creating named pipe {pipe_name}"))?;

    // Publish the pipe name only after we've successfully bound it —
    // otherwise a `bougie server list` race window would read a name
    // that doesn't yet correspond to a live listener.
    std::fs::write(&discovery_path, &pipe_name)
        .wrap_err_with(|| format!("writing pipe-discovery file {}", discovery_path.display()))?;

    let cleanup_path = discovery_path.clone();
    let pipe_name_for_loop = pipe_name.clone();
    let task = tokio::spawn(async move {
        // tokio's NamedPipeServer is single-instance: each `connect()`
        // produces one connection and consumes the listener. To accept
        // multiple clients we create a fresh instance for every
        // accepted connection and hand the connected instance to the
        // per-connection handler.
        let mut server = first;
        loop {
            if let Err(e) = server.connect().await {
                eprintln!("bougie control: pipe connect failed: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                match ServerOptions::new().create(&pipe_name_for_loop) {
                    Ok(s) => {
                        server = s;
                        continue;
                    }
                    Err(e2) => {
                        eprintln!("bougie control: recreate pipe failed: {e2}");
                        return;
                    }
                }
            }
            // Swap in a fresh instance for the next accept, hand the
            // connected one off to the handler task.
            let next = match ServerOptions::new().create(&pipe_name_for_loop) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("bougie control: recreate pipe failed: {e}");
                    return;
                }
            };
            let connected: NamedPipeServer = std::mem::replace(&mut server, next);
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                if let Err(e) = handle_connection(connected, state).await {
                    eprintln!("bougie control: connection error: {e:#}");
                }
            });
        }
    });

    Ok(ControlHandle { task, cleanup_path })
}

/// Derive a per-runtime-root named-pipe name. The hash isolates parallel
/// `bougie server` instances anchored at different `runtime_root`s
/// (e.g. test runs using a tempdir) so they don't collide on the global
/// `\\.\pipe\` namespace.
#[cfg(windows)]
fn windows_pipe_name(discovery_path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(discovery_path.to_string_lossy().as_bytes());
    let digest = h.finalize();
    let mut hex = String::with_capacity(12);
    for b in digest.iter().take(6) {
        hex.push_str(&format!("{b:02x}"));
    }
    format!(r"\\.\pipe\bougie-server-{hex}")
}

#[derive(Debug)]
pub struct ControlHandle {
    task: tokio::task::JoinHandle<()>,
    /// File path to remove on drop. On Unix this is the unix socket
    /// (a real filesystem entry); on Windows it's the discovery file
    /// holding the pipe name. The named pipe itself disappears when
    /// its last handle is dropped — no manual cleanup needed.
    pub cleanup_path: PathBuf,
}

impl ControlHandle {
    pub fn abort(&self) {
        self.task.abort();
    }
}

impl Drop for ControlHandle {
    fn drop(&mut self) {
        self.task.abort();
        // Best-effort cleanup. Surviving artifacts are harmless (the
        // next `start()` removes/overwrites them) but tidy is tidy.
        let _ = std::fs::remove_file(&self.cleanup_path);
    }
}

async fn handle_connection<S>(stream: S, state: Arc<AppState>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half).take(u64::try_from(MAX_REQUEST_BYTES).unwrap());
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .wrap_err("reading control request")?;
    if n == 0 {
        return Ok(()); // client closed without sending
    }

    let resp_json = match serde_json::from_str::<Request>(line.trim()) {
        Ok(req) => dispatch(req, &state).await,
        Err(e) => serde_json::to_string(&ErrorResponse::new(format!(
            "invalid request: {e}"
        )))
        .unwrap_or_else(|_| r#"{"schema_version":1,"ok":false,"error":"serialize"}"#.into()),
    };
    write_half.write_all(resp_json.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;
    Ok(())
}

async fn dispatch(req: Request, state: &Arc<AppState>) -> String {
    match req {
        Request::Status => {
            let rows = state.pools.status_snapshot().await;
            let mut hosts: Vec<String> = state
                .hosts
                .read()
                .map(|h| h.keys().cloned().collect())
                .unwrap_or_default();
            hosts.sort();
            let resp = StatusResponse {
                schema_version: 1,
                ok: true,
                listen_port: state.listen_port(),
                hosts,
                pools: rows
                    .into_iter()
                    .map(|r| PoolRow {
                        project: r.key.project,
                        variant: r.key.variant,
                        php_version: r.php_version,
                        pid: r.pid,
                        idle_ms: r.idle_ms,
                        started_ago_ms: r.started_ago_ms,
                    })
                    .collect(),
            };
            serde_json::to_string(&resp).unwrap_or_default()
        }
        Request::Reload { project } => match state.pools.reload_project(&project).await {
            Ok(n) => serde_json::to_string(&ReloadResponse {
                schema_version: 1,
                ok: true,
                reloaded_variants: n,
            })
            .unwrap_or_default(),
            Err(e) => serde_json::to_string(&ErrorResponse::new(e.to_string())).unwrap_or_default(),
        },
        Request::ReloadConfig => match reload_config(state) {
            Ok(hosts) => serde_json::to_string(&ReloadConfigResponse {
                schema_version: 1,
                ok: true,
                hosts,
            })
            .unwrap_or_default(),
            Err(e) => serde_json::to_string(&ErrorResponse::new(e.to_string())).unwrap_or_default(),
        },
    }
}

fn reload_config(state: &Arc<AppState>) -> Result<usize> {
    let cfg = super::config::load(&state.config_path)
        .wrap_err_with(|| format!("re-reading {}", state.config_path.display()))?;
    let count = state
        .replace_hosts(&cfg)
        .wrap_err("swapping in reloaded host map")?;
    Ok(count)
}

#[cfg(unix)]
fn set_socket_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .wrap_err_with(|| format!("chmod {} -> {mode:o}", path.display()))?;
    Ok(())
}

// --- client side ----------------------------------------------------

/// Try to query a running server for its status. The `path` is
/// platform-typed: on Unix it's the unix-socket path; on Windows it's
/// the discovery file under `runtime_root` that holds the named-pipe
/// name. Returns `None` silently when no server is listening — the
/// expected case for `bougie server list` without a live server.
#[cfg(unix)]
pub fn try_query_status(socket_path: &Path) -> Option<LiveStatus> {
    if !socket_path.exists() {
        return None;
    }
    // Run a one-shot tokio runtime; we don't want to pay for a
    // multithreaded one for what's a couple of syscalls.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async move {
        let socket_path = socket_path.to_owned();
        let fut = async {
            let stream = UnixStream::connect(&socket_path).await.ok()?;
            run_query(stream).await
        };
        tokio::time::timeout(Duration::from_millis(500), fut)
            .await
            .ok()
            .flatten()
    })
}

#[cfg(windows)]
pub fn try_query_status(discovery_path: &Path) -> Option<LiveStatus> {
    let pipe_name = std::fs::read_to_string(discovery_path)
        .ok()?
        .trim()
        .to_owned();
    if pipe_name.is_empty() {
        return None;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async move {
        let fut = async {
            let stream = ClientOptions::new().open(&pipe_name).ok()?;
            run_query(stream).await
        };
        tokio::time::timeout(Duration::from_millis(500), fut)
            .await
            .ok()
            .flatten()
    })
}

async fn run_query<S>(stream: S) -> Option<LiveStatus>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    write_half
        .write_all(b"{\"v\":1,\"method\":\"status\"}\n")
        .await
        .ok()?;
    // No explicit shutdown: handle_connection terminates the request
    // on the `\n` (read_line returns Ready), so signaling EOF isn't
    // needed and would tear down half-duplex named pipes early.
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    reader.read_line(&mut line).await.ok()?;
    serde_json::from_str::<LiveStatus>(line.trim()).ok()
}

/// Parsed status response, used by the `bougie server list` client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveStatus {
    pub schema_version: u32,
    pub ok: bool,
    pub listen_port: u16,
    pub hosts: Vec<String>,
    pub pools: Vec<LivePoolRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LivePoolRow {
    pub project: PathBuf,
    pub variant: String,
    pub php_version: String,
    pub pid: u32,
    pub idle_ms: u64,
    pub started_ago_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_status_parses() {
        let r: Request = serde_json::from_str(r#"{"v":1,"method":"status"}"#).unwrap();
        assert!(matches!(r, Request::Status));
    }

    #[test]
    fn request_reload_parses() {
        let r: Request = serde_json::from_str(
            r#"{"v":1,"method":"reload","project":"/tmp/p"}"#,
        )
        .unwrap();
        match r {
            Request::Reload { project } => assert_eq!(project, PathBuf::from("/tmp/p")),
            other => panic!("expected Reload, got {other:?}"),
        }
    }

    #[test]
    fn request_reload_config_parses() {
        let r: Request = serde_json::from_str(r#"{"v":1,"method":"reload-config"}"#).unwrap();
        assert!(matches!(r, Request::ReloadConfig));
    }

    #[test]
    fn unknown_method_errors() {
        assert!(serde_json::from_str::<Request>(r#"{"v":1,"method":"explode"}"#).is_err());
    }

    #[test]
    fn status_response_roundtrips() {
        let resp = StatusResponse {
            schema_version: 1,
            ok: true,
            listen_port: 7080,
            hosts: vec!["a.bougie.run".into()],
            pools: vec![PoolRow {
                project: PathBuf::from("/p"),
                variant: "normal".into(),
                php_version: "8.3.12-nts".into(),
                pid: 12345,
                idle_ms: 100,
                started_ago_ms: 5000,
            }],
        };
        let s = serde_json::to_string(&resp).unwrap();
        let live: LiveStatus = serde_json::from_str(&s).unwrap();
        assert_eq!(live.listen_port, 7080);
        assert_eq!(live.pools.len(), 1);
        assert_eq!(live.pools[0].pid, 12345);
    }

    #[cfg(unix)]
    #[test]
    fn try_query_status_returns_none_when_socket_missing() {
        let td = tempfile::TempDir::new().unwrap();
        let missing = td.path().join("absent.sock");
        assert!(try_query_status(&missing).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn try_query_status_returns_none_when_discovery_missing() {
        let td = tempfile::TempDir::new().unwrap();
        let missing = td.path().join("absent.pipe");
        assert!(try_query_status(&missing).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_pipe_name_is_hex_suffix_under_pipe_namespace() {
        let name = windows_pipe_name(Path::new(r"C:\tmp\control.pipe"));
        assert!(name.starts_with(r"\\.\pipe\bougie-server-"));
        assert_eq!(name.len(), r"\\.\pipe\bougie-server-".len() + 12);
    }
}
