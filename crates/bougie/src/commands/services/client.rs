//! Synchronous IPC client for `bougied`.
//!
//! The rest of bougie's CLI is sync; using a blocking `UnixStream`
//! against the daemon's tokio listener works fine. Tokio reads each
//! `\n`-terminated frame as the client writes it, and the client reads
//! frames the same way.
//!
//! On `ConnectionRefused` or missing-socket, the client auto-spawns
//! the daemon by exec'ing `current_exe()` with `argv[0] = "bougied"`
//! (the shim role wired in `src/shim.rs`). Auto-spawn is silent on the
//! happy path; the CLI emits a single "(starting bougied …)" line on
//! stderr so users understand the pause.

use bougie_paths::Paths;
use eyre::{eyre, Result, WrapErr};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long to wait for `bougied` to bind its socket after we spawn it.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);
const SPAWN_POLL: Duration = Duration::from_millis(50);

/// How long to wait for a daemon that we asked to shut down to release
/// its socket. The accept loop wakes on the watch channel and drops
/// `DaemonState`, which removes the socket file; in practice this is
/// well under 1s, but services in graceful drain can stretch it.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Once-per-CLI-invocation memo: we've either confirmed the running
/// daemon matches our version, or we've already shut a mismatched one
/// down. Either way, subsequent calls in the same process can skip
/// the check.
static VERSION_CHECKED: OnceLock<()> = OnceLock::new();

/// CLI's own reported version. Honors `BOUGIE_VERSION_OVERRIDE` so
/// integration tests can simulate an "old daemon, new CLI" mismatch
/// without rebuilding the binary. Production callers don't set it.
fn cli_version() -> String {
    std::env::var("BOUGIE_VERSION_OVERRIDE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

/// Client-side mirror of the daemon's `ResultFrame`. The daemon's
/// type is `Serialize`-only; this one is `Deserialize`-only.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ResponseFrame {
    Progress {
        #[serde(default)]
        stream: String,
        #[serde(default)]
        data: String,
    },
    Result {
        ok: bool,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<ErrorBody>,
    },
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

/// Issue a request to `bougied` and return the deserialized payload
/// of the terminal `result` frame. Progress frames are forwarded to
/// the caller's stderr (or stdout, per the frame's `stream` field)
/// before the result is returned.
///
/// Auto-spawns `bougied` if the socket is missing or refusing
/// connections.
pub fn call<R: DeserializeOwned>(paths: &Paths, method: &str, args: Value) -> Result<R> {
    ensure_compatible_daemon(paths, method)?;
    let sock = paths.bougied_sock();
    let stream = connect_with_autospawn(&sock)?;
    let request = serde_json::json!({"v": 1, "method": method, "args": args});
    issue(stream, &request)
}

/// Streaming variant of `call` for methods like `service.logs` where
/// the daemon emits indefinite progress frames (with no terminal
/// result, in follow mode). Forwards progress frames straight to
/// stdout/stderr and returns when the daemon either sends a terminal
/// frame or closes the connection. Auto-spawns the daemon.
pub fn call_streaming(paths: &Paths, method: &str, args: Value) -> Result<()> {
    ensure_compatible_daemon(paths, method)?;
    let sock = paths.bougied_sock();
    let stream = connect_with_autospawn(&sock)?;
    let request = serde_json::json!({"v": 1, "method": method, "args": args});
    issue_streaming(stream, &request)
}

fn connect_with_autospawn(sock: &Path) -> Result<UnixStream> {
    match UnixStream::connect(sock) {
        Ok(s) => Ok(s),
        Err(e)
            if e.kind() == ErrorKind::ConnectionRefused
                || e.kind() == ErrorKind::NotFound =>
        {
            // Stale socket from a daemon that exited abnormally
            // doesn't auto-clean — remove it before respawn.
            let _ = std::fs::remove_file(sock);
            spawn_daemon()?;
            wait_for_socket(sock, SPAWN_TIMEOUT)?;
            UnixStream::connect(sock)
                .wrap_err_with(|| format!("connecting to bougied at {}", sock.display()))
        }
        Err(e) => Err(eyre!(
            "connecting to bougied at {}: {e}",
            sock.display()
        )),
    }
}

/// Ensure the running daemon's version matches this CLI's. On
/// mismatch, send `daemon.shutdown` and wait for the socket to go
/// away so the next `connect_with_autospawn` brings up a fresh
/// daemon at the new version. Cached for the duration of one CLI
/// invocation — we pay the round-trip at most once.
///
/// `caller_method` skips the check for the two daemon-control
/// methods themselves: `daemon.version` is the probe we'd otherwise
/// loop on, and `daemon.shutdown` shouldn't trigger a restart of
/// the very thing the user just asked to stop.
fn ensure_compatible_daemon(paths: &Paths, caller_method: &str) -> Result<()> {
    if matches!(caller_method, "daemon.version" | "daemon.shutdown") {
        return Ok(());
    }
    if VERSION_CHECKED.get().is_some() {
        return Ok(());
    }
    let sock = paths.bougied_sock();
    if !sock.exists() {
        // No daemon. The autospawn path will start one at our
        // current version — by definition compatible.
        VERSION_CHECKED.set(()).ok();
        return Ok(());
    }
    let want = cli_version();
    match probe_daemon_version(&sock) {
        Some(daemon_ver) if daemon_ver == want => {
            VERSION_CHECKED.set(()).ok();
            Ok(())
        }
        Some(daemon_ver) => {
            eprintln!(
                "(restarting bougied: running v{daemon_ver}, cli v{want})"
            );
            send_shutdown(&sock).wrap_err("asking bougied to shut down for version upgrade")?;
            wait_for_socket_gone(&sock, SHUTDOWN_TIMEOUT)?;
            VERSION_CHECKED.set(()).ok();
            Ok(())
        }
        None => {
            // Couldn't reach the daemon or couldn't parse its reply.
            // Don't second-guess: maybe it's mid-shutdown or mid-spawn.
            // The subsequent autospawn path will sort it out.
            Ok(())
        }
    }
}

/// One-shot daemon.version round-trip used by the upgrade check.
/// Returns `None` if the daemon can't be reached or doesn't reply
/// with a parseable terminal frame — the caller treats that as
/// "skip the check, let autospawn retry."
fn probe_daemon_version(sock: &Path) -> Option<String> {
    let stream = UnixStream::connect(sock).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    let req = serde_json::json!({"v": 1, "method": "daemon.version"});
    let payload = serde_json::to_vec(&req).ok()?;
    {
        let mut w = &stream;
        w.write_all(&payload).ok()?;
        w.write_all(b"\n").ok()?;
        w.flush().ok()?;
    }
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let frame: ResponseFrame = serde_json::from_str(line.trim()).ok()?;
    match frame {
        ResponseFrame::Result { ok: true, result: Some(v), .. } => v
            .get("version")
            .and_then(|x| x.as_str())
            .map(std::string::ToString::to_string),
        _ => None,
    }
}

/// Send `daemon.shutdown` and best-effort read the reply. The
/// daemon may close the socket before flushing its terminal frame
/// on its way out — we don't surface that as an error.
fn send_shutdown(sock: &Path) -> Result<()> {
    let stream = UnixStream::connect(sock)
        .wrap_err_with(|| format!("connecting to bougied at {} for shutdown", sock.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .wrap_err("set_read_timeout on shutdown stream")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .wrap_err("set_write_timeout on shutdown stream")?;
    let req = serde_json::json!({"v": 1, "method": "daemon.shutdown"});
    let payload = serde_json::to_vec(&req).wrap_err("serializing shutdown")?;
    {
        let mut w = &stream;
        w.write_all(&payload).wrap_err("writing shutdown request")?;
        w.write_all(b"\n").wrap_err("writing shutdown terminator")?;
        w.flush().wrap_err("flushing shutdown request")?;
    }
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    Ok(())
}

/// Block until the daemon's socket stops accepting connections.
/// We poll because the daemon's drop path (which `unlink`s the
/// file) races with our shutdown reply.
fn wait_for_socket_gone(sock: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(sock) {
            Ok(s) => {
                drop(s);
                if Instant::now() >= deadline {
                    return Err(eyre!(
                        "timed out waiting for bougied to release {} after {:?}",
                        sock.display(),
                        timeout
                    ));
                }
                std::thread::sleep(SPAWN_POLL);
            }
            Err(_) => return Ok(()),
        }
    }
}

fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe().wrap_err("locating current bougie binary for auto-spawn")?;
    eprintln!("(starting bougied)");
    // arg0("bougied") triggers the shim role in `src/shim.rs`.
    // Null stdio so the daemon doesn't write to the CLI's tty. We
    // intentionally don't wait on the child — when the CLI exits,
    // init reparents and reaps. Phase 9 will add `setsid` for
    // proper detach across terminal close.
    let _child = std::process::Command::new(&exe)
        .arg0("bougied")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .wrap_err_with(|| format!("spawning bougied via {}", exe.display()))?;
    Ok(())
}

fn wait_for_socket(sock: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        // Probe with a real connect — file existence isn't enough
        // because the daemon `bind()`s before `chmod`, and we want
        // to wait until the listener is actually accepting.
        if let Ok(s) = UnixStream::connect(sock) {
            drop(s);
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(eyre!(
                "timed out waiting for bougied to bind {} (after {:?})",
                sock.display(),
                timeout
            ));
        }
        std::thread::sleep(SPAWN_POLL);
    }
}

fn issue_streaming(stream: UnixStream, request: &Value) -> Result<()> {
    {
        let mut writer = &stream;
        let payload = serde_json::to_vec(request).wrap_err("serializing request")?;
        writer.write_all(&payload).wrap_err("writing request")?;
        writer.write_all(b"\n").wrap_err("writing terminator")?;
        writer.flush().wrap_err("flushing request")?;
    }
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .wrap_err("reading streaming response")?;
        if n == 0 {
            return Ok(());
        }
        let frame: ResponseFrame = serde_json::from_str(line.trim())
            .wrap_err_with(|| format!("parsing frame: {}", line.trim()))?;
        match frame {
            ResponseFrame::Progress { stream, data } => if stream.as_str() == "stdout" {
                let _ = std::io::stdout().write_all(data.as_bytes());
                let _ = std::io::stdout().flush();
            } else {
                let _ = std::io::stderr().write_all(data.as_bytes());
                let _ = std::io::stderr().flush();
            },
            ResponseFrame::Result { ok: true, .. } => return Ok(()),
            ResponseFrame::Result { ok: false, error, .. } => {
                let e = error.unwrap_or(ErrorBody {
                    code: "unknown".into(),
                    message: "bougied returned an error without a body".into(),
                });
                return Err(eyre!("bougied: {} ({})", e.message, e.code));
            }
        }
    }
}

fn issue<R: DeserializeOwned>(stream: UnixStream, request: &Value) -> Result<R> {
    // Send: one request line.
    {
        let mut writer = &stream;
        let payload = serde_json::to_vec(request).wrap_err("serializing request")?;
        writer.write_all(&payload).wrap_err("writing request to bougied")?;
        writer.write_all(b"\n").wrap_err("writing request terminator")?;
        writer.flush().wrap_err("flushing request to bougied")?;
    }

    // Read frames until we hit a terminal `result` frame.
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .wrap_err("reading response from bougied")?;
        if n == 0 {
            return Err(eyre!("bougied closed connection without sending a result frame"));
        }
        let frame: ResponseFrame = serde_json::from_str(line.trim()).wrap_err_with(|| {
            format!("parsing response frame from bougied: {}", line.trim())
        })?;
        match frame {
            ResponseFrame::Progress { stream, data } => {
                // Forward unchanged. The daemon escapes embedded
                // newlines in `data` so a one-frame-per-line model
                // is preserved on the wire.
                match stream.as_str() {
                    "stdout" => {
                        let _ = std::io::stdout().write_all(data.as_bytes());
                    }
                    _ => {
                        let _ = std::io::stderr().write_all(data.as_bytes());
                    }
                }
            }
            ResponseFrame::Result { ok: true, result, .. } => {
                let value = result.unwrap_or(Value::Null);
                return serde_json::from_value(value)
                    .wrap_err("deserializing daemon result payload");
            }
            ResponseFrame::Result { ok: false, error, .. } => {
                let e = error.unwrap_or(ErrorBody {
                    code: "unknown".into(),
                    message: "bougied returned an error without a body".into(),
                });
                return Err(eyre!("bougied: {} ({})", e.message, e.code));
            }
        }
    }
}
