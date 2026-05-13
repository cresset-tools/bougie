//! Control socket: phase 6 of SERVER_PLAN.md. Lets `bougie server list`
//! query a running server for live pool state.
//!
//! Wire format is one JSON object per request and one JSON object per
//! response, both terminated by `\n`. No framing, no auth — the socket
//! is mode 0600 so only the owner can connect.
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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::router::AppState;

/// Max bytes a client may send before we close the connection. The
/// `reload` method is the heaviest user and its payload is a path —
/// 4 KB is plenty even for unusually-deep project locations.
const MAX_REQUEST_BYTES: usize = 4096;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum Request {
    Status,
    Reload { project: PathBuf },
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

/// Start the control socket listener. Returns a `JoinHandle` plus the
/// socket path so the server's shutdown path can clean up. Aborting the
/// handle stops the accept loop; any in-flight handlers see the socket
/// closed mid-write.
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

    let socket_for_handle = socket_path.clone();
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

    Ok(ControlHandle { task, socket_path: socket_for_handle })
}

#[derive(Debug)]
pub struct ControlHandle {
    task: tokio::task::JoinHandle<()>,
    pub socket_path: PathBuf,
}

impl ControlHandle {
    pub fn abort(&self) {
        self.task.abort();
    }
}

impl Drop for ControlHandle {
    fn drop(&mut self) {
        self.task.abort();
        // Best-effort socket cleanup. Surviving sockets are harmless
        // (the next `start()` call removes them) but tidy is tidy.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

async fn handle_connection(stream: UnixStream, state: Arc<AppState>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
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
            let mut hosts: Vec<String> = state.hosts.keys().cloned().collect();
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
    }
}

fn set_socket_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .wrap_err_with(|| format!("chmod {} -> {mode:o}", path.display()))?;
    Ok(())
}

// --- client side ----------------------------------------------------

/// Try to query a running server's control socket for its status.
/// Returns `None` (silently) when no server is listening — the
/// expected case for `bougie server list` without a live server.
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
            let (read_half, mut write_half) = stream.into_split();
            write_half.write_all(b"{\"v\":1,\"method\":\"status\"}\n").await.ok()?;
            write_half.shutdown().await.ok()?;
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            reader.read_line(&mut line).await.ok()?;
            serde_json::from_str::<LiveStatus>(line.trim()).ok()
        };
        tokio::time::timeout(Duration::from_millis(500), fut)
            .await
            .ok()
            .flatten()
    })
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
            Request::Status => panic!("expected Reload"),
        }
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

    #[test]
    fn try_query_status_returns_none_when_socket_missing() {
        let td = tempfile::TempDir::new().unwrap();
        let missing = td.path().join("absent.sock");
        assert!(try_query_status(&missing).is_none());
    }
}
