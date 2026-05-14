//! Synchronous IPC client for `bougied`.
//!
//! The rest of bougie's CLI is sync; using a blocking UnixStream
//! against the daemon's tokio listener works fine. Tokio reads each
//! `\n`-terminated frame as the client writes it, and the client reads
//! frames the same way.
//!
//! On `ConnectionRefused` or missing-socket, the client auto-spawns
//! the daemon by exec'ing `current_exe()` with `argv[0] = "bougied"`
//! (the shim role wired in `src/shim.rs`). Auto-spawn is silent on the
//! happy path; the CLI emits a single "(starting bougied …)" line on
//! stderr so users understand the pause.

use crate::Paths;
use eyre::{eyre, Result, WrapErr};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long to wait for `bougied` to bind its socket after we spawn it.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);
const SPAWN_POLL: Duration = Duration::from_millis(50);

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
            ResponseFrame::Progress { stream, data } => match stream.as_str() {
                "stdout" => {
                    let _ = std::io::stdout().write_all(data.as_bytes());
                    let _ = std::io::stdout().flush();
                }
                _ => {
                    let _ = std::io::stderr().write_all(data.as_bytes());
                    let _ = std::io::stderr().flush();
                }
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
