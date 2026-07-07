//! Line-delimited JSON IPC for `bougied`.
//!
//! Wire format mirrors the existing `bougie server` control socket
//! (`src/commands/server/control.rs`) so the two daemons feel
//! consistent to operators:
//!
//! Request:  `{"v": 1, "method": "<name>", "args": {...}}\n`
//!
//! Response: zero or more `progress` / `download` frames followed by
//! exactly one `result` frame; all `\n`-terminated.
//!
//! ```jsonc
//! {"schema_version": 1, "type": "progress", "stream": "stderr", "data": "…"}
//! {"schema_version": 1, "type": "download", "pos": 12345, "total": 67890, "label": "opensearch-2.19.5"}
//! {"schema_version": 1, "type": "result",   "ok": true,  "result": {...}}
//! {"schema_version": 1, "type": "result",   "ok": false, "error": {"code": "...", "message": "..."}}
//! ```
//!
//! Schema version is `1` for v1 of the supervisor (SERVICES.md §7).
//! New methods may be added without bumping it; removed or
//! semantically-changed methods MUST bump it.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::watch;

use super::state::DaemonState;

/// Schema version stamped on every response frame. Bumped on
/// breaking changes to existing method shapes.
pub const SCHEMA_VERSION: u32 = 1;

/// What `daemon.version` reports as the daemon's running version.
/// Honors `BOUGIE_VERSION_OVERRIDE` so the integration test suite
/// can stage a CLI-vs-daemon mismatch without rebuilding the binary;
/// production users would never set this.
fn daemon_version_string() -> String {
    std::env::var("BOUGIE_VERSION_OVERRIDE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

/// Max bytes a single request line may contain. Generous because
/// future `service.up` calls carry project paths and service lists.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

// -------------------- Request side --------------------

/// Raw wire envelope: `{"v": 1, "method": "...", "args": {...}}`.
/// Method-specific args are pulled out of `args` after the method
/// string dispatches to a variant.
#[derive(Debug, Deserialize)]
pub struct RequestEnvelope {
    #[serde(rename = "v")]
    pub version: u32,
    pub method: String,
    #[serde(default)]
    pub args: Value,
}

/// Method-specific deserialized request. `Status` / `DaemonVersion` /
/// `DaemonShutdown` / `Catalog` carry no args; the others pull
/// their fields out of the envelope's `args` object.
#[derive(Debug)]
pub enum Request {
    Status,
    DaemonVersion,
    DaemonShutdown,
    ServiceUp(ServiceUpArgs),
    ServiceDown(ServiceDownArgs),
    /// Stop + start the named services without touching the tenant
    /// ledger. SERVICES.md §7.2.
    ServiceRestart(ServiceRestartArgs),
    /// Used by `bougie run` to pick up tenant-derived env vars to
    /// inject into the child PHP process. Idempotent + side-effect-free.
    ServiceEnv(ServiceEnvArgs),
    /// Tail (and optionally follow) a service's log.
    ServiceLogs(ServiceLogsArgs),
    /// Read-only: returns the in-binary catalog as JSON. Mirrors what
    /// `bougie service catalog` shows locally; exposed via IPC for
    /// external tooling.
    Catalog,
}

#[derive(Debug, Deserialize)]
pub struct ServiceUpArgs {
    pub project: std::path::PathBuf,
    pub services: Vec<ServiceRequest>,
}

#[derive(Debug, Deserialize)]
pub struct ServiceDownArgs {
    pub project: std::path::PathBuf,
    pub services: Vec<String>,
    #[serde(default)]
    pub purge: bool,
}

#[derive(Debug, Deserialize)]
pub struct ServiceRestartArgs {
    pub project: std::path::PathBuf,
    /// Same shape as `service.down`: a list of catalog names. Empty
    /// means "every declared service" — but the CLI resolves that
    /// against the project's config before the IPC call, so an empty
    /// vec on the daemon side is a no-op.
    pub services: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ServiceEnvArgs {
    pub project: std::path::PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct ServiceLogsArgs {
    /// Single-service form (`bougie service logs <name>`). Mutually
    /// exclusive with `services`; if both are present `services` wins.
    #[serde(default)]
    pub service: Option<String>,
    /// Multi-service form (`bougie up`'s combined stream). When more
    /// than one name is present, each emitted line is prefixed with the
    /// service name so the streams stay distinguishable.
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default = "default_lines")]
    pub lines: usize,
    #[serde(default)]
    pub follow: bool,
    /// Colorize the per-service prefix in the multi-service stream with
    /// ANSI codes. The CLI sets this only when its own stdout is a TTY —
    /// the daemon can't tell, and we don't want escapes in a redirected
    /// or piped `bougie up`. No effect on the single-service form.
    #[serde(default)]
    pub color: bool,
    /// Restrict the stream to log lines containing this substring (the
    /// `<name>.bougie.run` vhost). Set by `bougie server` / `bougie
    /// server logs` to scope the shared dev-server log to one project.
    /// Honoured only by the single-service form; ignored for the
    /// multi-service combined stream.
    #[serde(default)]
    pub host: Option<String>,
}

impl ServiceLogsArgs {
    /// Normalise the single/multi forms into one ordered list. Prefers
    /// the explicit `services` array; otherwise the lone `service`.
    fn service_names(&self) -> Vec<String> {
        if self.services.is_empty() {
            self.service.clone().into_iter().collect()
        } else {
            self.services.clone()
        }
    }
}

fn default_lines() -> usize {
    50
}

#[derive(Debug, Deserialize)]
pub struct ServiceRequest {
    pub name: String,
    pub tenant: String,
}

// -------------------- Response side --------------------

/// `progress` frame — streamed before the terminal `result`.
///
/// `stream` is `"stdout"` or `"stderr"` so the CLI can route the
/// bytes to the matching fd on its side.
#[derive(Debug, Serialize)]
pub struct ProgressFrame<'a> {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub typ: &'static str,
    pub stream: &'a str,
    pub data: &'a str,
}

impl<'a> ProgressFrame<'a> {
    pub fn new(stream: &'a str, data: &'a str) -> Self {
        Self { schema_version: SCHEMA_VERSION, typ: "progress", stream, data }
    }
}

/// `download` frame — streamed during `service.up` to surface tarball
/// fetch progress to the CLI. Carries the *aggregate* state of the
/// shared [`bougie_fetch::DownloadBar`] (planned total, bytes so far,
/// label of the artifact currently in flight) rather than per-byte
/// deltas, so the wire bandwidth is bounded by the daemon's emit
/// cadence (~50ms) regardless of how fast the underlying transfer is.
///
/// The CLI's mirror of this frame drives a local
/// `bougie_fetch::DownloadBar` so the user sees the exact same widget
/// they'd see from an in-process fetch (extension install, baseline
/// PHP fetch). Old CLIs that don't know this frame type fall over at
/// the version-skew check and trigger a daemon restart before they
/// ever see one.
#[derive(Debug, Serialize)]
pub struct DownloadFrame<'a> {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub typ: &'static str,
    pub pos: u64,
    pub total: u64,
    pub label: &'a str,
    /// `true` once the artifact in `label` has finished downloading and
    /// is being extracted. Lets the CLI's mirrored bar flip its prefix
    /// to `extracting` the same way an in-process bar does, instead of
    /// freezing at `N/N bytes` through the silent decompress. Defaults
    /// to `false` on the CLI side, so an older daemon that omits it
    /// degrades to the previous (download-only) labelling.
    pub extracting: bool,
}

impl<'a> DownloadFrame<'a> {
    pub fn new(pos: u64, total: u64, label: &'a str, extracting: bool) -> Self {
        Self { schema_version: SCHEMA_VERSION, typ: "download", pos, total, label, extracting }
    }
}

/// Terminal `result` frame — exactly one per request.
#[derive(Debug, Serialize)]
pub struct ResultFrame {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub typ: &'static str,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl ResultFrame {
    pub fn ok(result: Value) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            typ: "result",
            ok: true,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            typ: "result",
            ok: false,
            result: None,
            error: Some(ErrorBody { code: code.into(), message: message.into() }),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

// -------------------- Connection handling --------------------

/// Read one request line, dispatch it, write the response frames,
/// flush, return. Errors here are connection-local: log and close.
pub async fn handle_connection(stream: UnixStream, state: Arc<DaemonState>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half).take(MAX_REQUEST_BYTES);
    let mut line = String::new();

    let read = match reader.read_line(&mut line).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "bougied: reading request line");
            return;
        }
    };
    if read == 0 {
        // Client closed before sending.
        return;
    }

    match parse_request(line.trim()) {
        // Streaming methods write their own progress frames + a
        // terminal frame (or, for `service.logs --follow`, no terminal
        // at all — the client closing the socket is the exit signal).
        Ok(Request::ServiceLogs(args)) => {
            dispatch_logs(&mut write_half, &state, args).await;
        }
        Ok(Request::ServiceUp(args)) => {
            dispatch_up_streaming(&mut write_half, &state, args).await;
        }
        // Streaming: a `stopping`/`starting` progress pair per service as
        // it cycles, then the terminal `result`.
        Ok(Request::ServiceRestart(args)) => {
            dispatch_restart_streaming(&mut write_half, &state, args.services).await;
        }
        // Streaming drain: a `progress` frame per service as it stops,
        // then the terminal `result`, then the daemon tears itself down.
        Ok(Request::DaemonShutdown) => {
            dispatch_shutdown_streaming(&mut write_half, &state).await;
        }
        Ok(req) => {
            let frame = dispatch(req, &state).await;
            write_terminal(&mut write_half, &frame).await;
        }
        Err(e) => {
            let frame = ResultFrame::err("bad_request", e);
            write_terminal(&mut write_half, &frame).await;
        }
    }
}

async fn write_terminal(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    frame: &ResultFrame,
) {
    let bytes = match serde_json::to_vec(frame) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "bougied: serializing response");
            return;
        }
    };
    if let Err(e) = write_half.write_all(&bytes).await {
        tracing::warn!(error = %e, "bougied: writing response");
        return;
    }
    if let Err(e) = write_half.write_all(b"\n").await {
        tracing::warn!(error = %e, "bougied: writing response terminator");
        return;
    }
    let _ = write_half.flush().await;
}

fn parse_request(line: &str) -> Result<Request, String> {
    let env: RequestEnvelope = serde_json::from_str(line)
        .map_err(|e| format!("malformed request envelope: {e}"))?;
    if env.version != 1 {
        return Err(format!(
            "unsupported wire-protocol version `v={}` (expected 1)",
            env.version
        ));
    }
    match env.method.as_str() {
        "status" => Ok(Request::Status),
        "daemon.version" => Ok(Request::DaemonVersion),
        "daemon.shutdown" => Ok(Request::DaemonShutdown),
        "service.up" => serde_json::from_value::<ServiceUpArgs>(env.args)
            .map(Request::ServiceUp)
            .map_err(|e| format!("service.up args: {e}")),
        "service.down" => serde_json::from_value::<ServiceDownArgs>(env.args)
            .map(Request::ServiceDown)
            .map_err(|e| format!("service.down args: {e}")),
        "service.restart" => serde_json::from_value::<ServiceRestartArgs>(env.args)
            .map(Request::ServiceRestart)
            .map_err(|e| format!("service.restart args: {e}")),
        "catalog" => Ok(Request::Catalog),
        "service.env" => serde_json::from_value::<ServiceEnvArgs>(env.args)
            .map(Request::ServiceEnv)
            .map_err(|e| format!("service.env args: {e}")),
        "service.logs" => serde_json::from_value::<ServiceLogsArgs>(env.args)
            .map(Request::ServiceLogs)
            .map_err(|e| format!("service.logs args: {e}")),
        other => Err(format!("unknown method `{other}`")),
    }
}

async fn dispatch(req: Request, state: &Arc<DaemonState>) -> ResultFrame {
    match req {
        Request::Status => {
            let snap = state.supervisor.lock().await.snapshot();
            ResultFrame::ok(
                serde_json::to_value(serde_json::json!({"services": snap}))
                    .unwrap_or(Value::Null),
            )
        }
        Request::DaemonVersion => ResultFrame::ok(serde_json::json!({
            "version": daemon_version_string(),
            "build_hash": option_env!("BOUGIE_BUILD_HASH").unwrap_or(""),
        })),
        Request::DaemonShutdown => unreachable!("handled in handle_connection"),
        Request::ServiceUp(args) => dispatch_up(state, args.project, args.services, None).await,
        Request::ServiceDown(args) => {
            dispatch_down(state, args.project, args.services, args.purge).await
        }
        Request::ServiceRestart(_) => unreachable!("handled in handle_connection"),
        Request::ServiceEnv(args) => dispatch_env(state, args.project).await,
        Request::ServiceLogs(_) => unreachable!("handled in handle_connection"),
        Request::Catalog => dispatch_catalog(),
    }
}

/// Render the in-binary catalog as a JSON value.
///
/// The CLI's `bougie service catalog` reads `catalog::CATALOG`
/// directly (it's a `const`) — auto-spawning bougied just to print
/// a static list would be terrible UX. This method exists for
/// external tooling consumers and SERVICES.md §7.2 spec compliance.
fn dispatch_catalog() -> ResultFrame {
    let entries = super::catalog::CATALOG;
    match serde_json::to_value(entries) {
        Ok(v) => ResultFrame::ok(serde_json::json!({"catalog": v})),
        Err(e) => ResultFrame::err("serialize", format!("catalog: {e}")),
    }
}

/// Restart each service in topological order. Each is `stop` then
/// `start` — both supervisor methods are idempotent and the
/// `Mutex<Supervisor>` serialises the pair so no concurrent
/// `service.up` can wedge in. The tenant ledger is left alone.
///
/// Streams a `stopping {name}` progress frame before each stop and a
/// `starting {name}` frame before each (re)start, so the user sees the
/// cycle live instead of staring at a frozen prompt. Mirrors the
/// stderr progress convention of [`dispatch_shutdown_streaming`]. The
/// terminal `result` still carries the `restarted` list that drives the
/// CLI's final summary.
async fn dispatch_restart_streaming(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    state: &Arc<DaemonState>,
    services: Vec<String>,
) {
    use crate::daemon::catalog;
    let names: Vec<&str> = services.iter().map(std::string::String::as_str).collect();
    // Re-order to respect after/requires graph. Same topology used
    // by `service.up` so a `restart` of dependents lines up the same
    // way as a fresh boot.
    let order = match super::supervisor::compute_start_order(&names) {
        Ok(o) => o,
        Err(e) => {
            write_terminal(write_half, &ResultFrame::err("bad_request", e.to_string())).await;
            return;
        }
    };
    let mut restarted = Vec::new();
    for name in order {
        // Skip transitively-pulled runtime deps that aren't real
        // managed processes (jdk, erlang).
        if !catalog::find(name).is_some_and(|e| e.user_facing) {
            continue;
        }
        let _ = write_progress(write_half, "stderr", &format!("stopping {name}\n")).await;
        // `stop` returns Ok(false) when the service wasn't running;
        // skip those — `restart` of a stopped service is a no-op,
        // matching `systemctl restart` semantics. Scope the guard so the
        // lock is released before the (off-lock) start below.
        let was_running = {
            let mut sup = state.supervisor.lock().await;
            match sup.stop(name).await {
                Ok(true) => true,
                Ok(false) => false,
                Err(e) => {
                    drop(sup);
                    write_terminal(
                        write_half,
                        &ResultFrame::err("service_stop_failed", format!("{name}: {e}")),
                    )
                    .await;
                    return;
                }
            }
        };
        if was_running {
            let _ = write_progress(write_half, "stderr", &format!("starting {name}\n")).await;
            if let Err(e) = super::supervisor::start_service(&state.supervisor, name).await {
                write_terminal(
                    write_half,
                    &ResultFrame::err("service_start_failed", format!("{name}: {e}")),
                )
                .await;
                return;
            }
            restarted.push(name.to_string());
        }
    }
    write_terminal(
        write_half,
        &ResultFrame::ok(serde_json::json!({"restarted": restarted})),
    )
    .await;
}

/// Streaming `service.logs` handler. Reads the initial tail, then
/// either sends a terminal `result` (tail-only) or enters follow-mode
/// — a 250ms poll loop that streams new bytes as `progress` frames.
/// Follow-mode never sends a terminal; the client closing the socket
/// is the signal to exit.
async fn dispatch_logs(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    state: &Arc<DaemonState>,
    args: ServiceLogsArgs,
) {
    use crate::daemon::catalog;

    let names = args.service_names();
    if names.is_empty() {
        let frame = ResultFrame::err("bad_request", "no service named for logs");
        write_terminal(write_half, &frame).await;
        return;
    }

    // Resolve every name up front so an unknown service fails the whole
    // request rather than half-streaming. Keep the catalog's `'static`
    // name (the request may differ only in case / aliasing).
    let mut targets: Vec<(&'static str, std::path::PathBuf)> = Vec::with_capacity(names.len());
    for name in &names {
        let Some(entry) = catalog::find(name) else {
            let frame =
                ResultFrame::err("unknown_service", format!("`{name}` not in catalog"));
            write_terminal(write_half, &frame).await;
            return;
        };
        let log_path = state.paths.service_log_file(entry.name);
        targets.push((entry.name, log_path));
    }

    if targets.len() == 1 {
        let (_, log_path) = &targets[0];
        dispatch_logs_single(write_half, log_path, args.lines, args.follow, args.host.as_deref())
            .await;
    } else {
        dispatch_logs_multi(write_half, &targets, args.lines, args.follow, args.color).await;
    }
}

/// Drain every newline-terminated line from `partial`, returning those
/// that match `host` (when set) concatenated in order. A trailing
/// partial line (no newline yet) is left in `partial` for the next
/// read. With `host == None` every complete line passes through.
fn drain_matching_lines(partial: &mut Vec<u8>, host: Option<&str>) -> String {
    let mut out = String::new();
    while let Some(nl) = partial.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = partial.drain(..=nl).collect();
        let line = String::from_utf8_lossy(&line);
        match host {
            Some(h) if !line.contains(h) => {}
            _ => out.push_str(&line),
        }
    }
    out
}

/// Single-service tail/follow — the original `service logs <name>`
/// behaviour: raw byte chunks, no per-line prefix.
async fn dispatch_logs_single(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    log_path: &std::path::Path,
    lines: usize,
    follow: bool,
    host: Option<&str>,
) {
    use crate::daemon::logs;

    // 1. Tail. When a host filter is set, keep only matching lines —
    //    tail-then-filter, so the tail may show fewer than `lines`.
    let tail = logs::tail_lines(log_path, lines).unwrap_or_default();
    let tail: Vec<String> = match host {
        Some(h) => tail.into_iter().filter(|l| l.contains(h)).collect(),
        None => tail,
    };
    let joined = tail.concat();
    if !joined.is_empty()
        && !write_progress(write_half, "stdout", &joined).await {
            return;
        }

    if !follow {
        let frame = ResultFrame::ok(serde_json::json!({"lines_tailed": tail.len()}));
        write_terminal(write_half, &frame).await;
        return;
    }

    // 2. Follow: seek to current end-of-file, poll for growth, stream
    // new bytes. Buffer reused across iterations to avoid alloc churn.
    let mut f = match tokio::fs::OpenOptions::new().read(true).open(log_path).await {
        Ok(f) => f,
        Err(e) => {
            let frame = ResultFrame::err("log_open_failed", e.to_string());
            write_terminal(write_half, &frame).await;
            return;
        }
    };
    if f.seek(std::io::SeekFrom::End(0)).await.is_err() {
        return;
    }
    let mut buf = vec![0u8; 8 * 1024];
    // Only used in the filtered path: holds back a trailing partial line
    // so the host filter always sees whole lines.
    let mut partial: Vec<u8> = Vec::new();
    loop {
        match f.read(&mut buf).await {
            Ok(0) => {
                // EOF — but this may be a rotation rather than idleness:
                // LogWriter renames the file and opens a fresh one, leaving
                // our fd pinned to the now-idle old inode (endless EOF).
                // Reopen the new file from its start when the path's inode
                // changed; otherwise sleep and retry.
                if let Some(reopened) = reopen_if_rotated(log_path, &f).await {
                    f = reopened;
                } else {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
            Ok(n) => {
                // Unfiltered: raw chunk passthrough (byte-identical to the
                // original behaviour). Filtered: line-buffer and emit only
                // matching complete lines.
                let emitted = if host.is_some() {
                    partial.extend_from_slice(&buf[..n]);
                    drain_matching_lines(&mut partial, host)
                } else {
                    String::from_utf8_lossy(&buf[..n]).into_owned()
                };
                if !emitted.is_empty() && !write_progress(write_half, "stdout", &emitted).await {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Reopen a followed log file if it was rotated out from under us.
///
/// `LogWriter` rotates by renaming `svc.log` → `svc.log.1` and opening a
/// fresh `svc.log`, so a follower's fd stays pinned to the renamed inode
/// — which never grows again, yielding endless `Ok(0)` (EOF), *not* an
/// error. On EOF we therefore compare the open file's `(dev, inode)`
/// against the path's; if they differ the file was rotated (or replaced),
/// so reopen the new file positioned at its start (no seek-to-end) to
/// resume following without dropping the rotated-in lines. Returns `None`
/// when nothing changed (ordinary idle EOF) or the path can't be
/// stat'd/opened yet.
async fn reopen_if_rotated(
    path: &std::path::Path,
    current: &tokio::fs::File,
) -> Option<tokio::fs::File> {
    use std::os::unix::fs::MetadataExt;
    let cur = current.metadata().await.ok()?;
    let on_disk = tokio::fs::metadata(path).await.ok()?;
    if (cur.dev(), cur.ino()) == (on_disk.dev(), on_disk.ino()) {
        return None;
    }
    tokio::fs::OpenOptions::new().read(true).open(path).await.ok()
}

/// ANSI 16-color foreground codes cycled across services in the
/// combined stream, so each service's prefix is a distinct color (à la
/// `docker compose up`). Chosen to read on both light and dark
/// terminals and to skip the 30/37 ends (black / white / grey) that
/// vanish against common backgrounds. Cycles if there are more services
/// than colors.
const PREFIX_COLORS: &[u8] = &[36, 33, 32, 35, 34, 31, 96, 93, 92, 95, 94, 91];

/// One followed log file in the multi-service stream. `file` is lazily
/// (re)opened so a service whose log doesn't exist yet — or that rotates
/// out from under us — is tolerated rather than fatal.
#[derive(Debug)]
struct MultiTailer {
    name: &'static str,
    path: std::path::PathBuf,
    /// ANSI color code for this service's prefix, or `None` when color
    /// is off (non-TTY CLI). Stable per service for the whole stream.
    color: Option<u8>,
    file: Option<tokio::fs::File>,
    /// Bytes received since the last newline; held back so we only ever
    /// prefix whole lines.
    partial: Vec<u8>,
}

/// Multi-service combined stream. Each emitted line is prefixed with the
/// service name (left-padded to the widest name) so `bougie up`'s merged
/// follow stays readable, à la `docker compose up`. When `color` is set,
/// each service's prefix gets a distinct ANSI color.
async fn dispatch_logs_multi(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    targets: &[(&'static str, std::path::PathBuf)],
    lines: usize,
    follow: bool,
    color: bool,
) {
    use crate::daemon::logs;

    let width = targets.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    // Assign each service a stable color by its position in the list.
    let color_for = |idx: usize| -> Option<u8> {
        color.then(|| PREFIX_COLORS[idx % PREFIX_COLORS.len()])
    };

    // 1. Tail each service in turn, prefixing every line.
    let mut tailed = 0usize;
    for (idx, (name, path)) in targets.iter().enumerate() {
        let tail = logs::tail_lines(path, lines).unwrap_or_default();
        tailed += tail.len();
        let mut out = String::new();
        for line in &tail {
            prefix_line(&mut out, name, width, color_for(idx), line);
        }
        if !out.is_empty() && !write_progress(write_half, "stdout", &out).await {
            return;
        }
    }

    if !follow {
        let frame = ResultFrame::ok(serde_json::json!({"lines_tailed": tailed}));
        write_terminal(write_half, &frame).await;
        return;
    }

    // 2. Follow: open each file (seek to EOF), then round-robin poll for
    // growth. A single task owns every file, so there's no contention on
    // `write_half` and no locking. Sleep only when no file had new bytes.
    let mut tailers: Vec<MultiTailer> = targets
        .iter()
        .enumerate()
        .map(|(idx, (name, path))| MultiTailer {
            name,
            path: path.clone(),
            color: color_for(idx),
            file: None,
            partial: Vec::new(),
        })
        .collect();
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        let mut any_progress = false;
        for t in &mut tailers {
            if t.file.is_none() {
                // Newly-started services may not have created their log
                // yet; retry next tick. Seek to EOF on first open so we
                // only stream output that lands after we attach.
                if let Ok(mut f) =
                    tokio::fs::OpenOptions::new().read(true).open(&t.path).await
                {
                    let _ = f.seek(std::io::SeekFrom::End(0)).await;
                    t.file = Some(f);
                } else {
                    continue;
                }
            }
            let Some(f) = t.file.as_mut() else { continue };
            match f.read(&mut buf).await {
                Ok(0) => {
                    // EOF. A rotation (rename + fresh reopen) yields endless
                    // EOF, not an error, so the `Err` arm below never fires
                    // for it — reopen the new file from its start when the
                    // path's inode changed.
                    if let Some(reopened) = reopen_if_rotated(&t.path, f).await {
                        t.file = Some(reopened);
                        any_progress = true;
                    }
                }
                Ok(n) => {
                    any_progress = true;
                    t.partial.extend_from_slice(&buf[..n]);
                    let mut out = String::new();
                    while let Some(idx) = t.partial.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = t.partial.drain(..=idx).collect();
                        let text = String::from_utf8_lossy(&line);
                        prefix_line(&mut out, t.name, width, t.color, &text);
                    }
                    if !out.is_empty() && !write_progress(write_half, "stdout", &out).await {
                        return;
                    }
                }
                // Rotation is handled on EOF above; a genuine read error
                // is the fallback — drop the handle so we reopen next tick.
                Err(_) => t.file = None,
            }
        }
        if !any_progress {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

/// Append `line` to `out` with a `name | ` prefix, the name left-padded
/// to `width`. `line` already carries its own trailing newline (or is
/// the file's final unterminated fragment). When `color` is `Some`, the
/// `name |` prefix is wrapped in that ANSI foreground color and reset
/// before the log text, so only the prefix is tinted.
fn prefix_line(out: &mut String, name: &str, width: usize, color: Option<u8>, line: &str) {
    use std::fmt::Write as _;
    match color {
        Some(c) => {
            let _ = write!(out, "\x1b[{c}m{name:<width$} |\x1b[0m {line}");
        }
        None => {
            let _ = write!(out, "{name:<width$} | {line}");
        }
    }
}

async fn write_progress(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    stream: &str,
    data: &str,
) -> bool {
    let frame = ProgressFrame::new(stream, data);
    let bytes = match serde_json::to_vec(&frame) {
        Ok(b) => b,
        Err(_) => return false,
    };
    if write_half.write_all(&bytes).await.is_err() {
        return false;
    }
    if write_half.write_all(b"\n").await.is_err() {
        return false;
    }
    write_half.flush().await.is_ok()
}

async fn write_download(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    pos: u64,
    total: u64,
    label: &str,
    extracting: bool,
) -> bool {
    let frame = DownloadFrame::new(pos, total, label, extracting);
    let bytes = match serde_json::to_vec(&frame) {
        Ok(b) => b,
        Err(_) => return false,
    };
    if write_half.write_all(&bytes).await.is_err() {
        return false;
    }
    if write_half.write_all(b"\n").await.is_err() {
        return false;
    }
    write_half.flush().await.is_ok()
}

/// IPC-side `DownloadSink`: pushes every `DownloadBar` event onto an
/// unbounded mpsc; the streaming dispatcher consumes, coalesces, and
/// throttles before serializing to the wire.
///
/// Unbounded is the right tradeoff here: the producer side (the
/// blocking fetch thread) can't await, so a bounded `send` that
/// blocks would either stall the download or have to be `try_send`'d
/// with an "events dropped" fallback — and dropping Inc events would
/// make the bar's position drift. The aggregate event volume per
/// download is small enough (one Inc per 64KB chunk, plus a handful
/// of Plan/Current/Finish bookends) that unbounded queueing is fine.
#[derive(Debug)]
struct IpcDownloadSink {
    tx: tokio::sync::mpsc::UnboundedSender<bougie_fetch::DownloadEvent>,
}

impl bougie_fetch::DownloadSink for IpcDownloadSink {
    fn on_event(&self, event: bougie_fetch::DownloadEvent) {
        // Failure means the consumer task has dropped its receiver
        // (e.g. the CLI hung up mid-fetch). The fetch itself keeps
        // running — we don't have a clean cancellation path — but
        // suppressing further events is correct.
        let _ = self.tx.send(event);
    }
}

/// Streaming wrapper around [`dispatch_up`] that surfaces tarball
/// download progress to the CLI as `download` frames.
///
/// Sets up an unbounded mpsc + a shared [`bougie_fetch::DownloadBar`]
/// with an [`IpcDownloadSink`], then runs the existing `dispatch_up`
/// future. Every ~50ms (or when the future completes), the loop
/// drains pending events, applies them to a running snapshot, and —
/// if the snapshot changed — writes one `download` frame. The final
/// terminal `result` is written last, identical to the non-streaming
/// path.
///
/// 50ms is below indicatif's own 15Hz redraw cadence on the CLI side,
/// so the user sees a smooth bar; it's also high enough that even a
/// 100MB/s LAN transfer (≈1600 `inc` events/s) collapses into one
/// frame per tick instead of one per chunk.
/// Every loopback TCP port a service occupies — the primary
/// [`Binding::Tcp`] port plus any secondary ports the single-endpoint
/// `Binding` can't model. Today the only multi-port service is mailpit,
/// which binds SMTP ([`catalog::MAILPIT_SMTP_PORT`], its catalog binding)
/// *and* a web UI / REST API ([`catalog::MAILPIT_HTTP_PORT`]) that rides
/// alongside. Returns empty for unix-socket services + runtime-only deps.
fn service_tcp_ports(entry: &crate::daemon::catalog::CatalogEntry) -> Vec<u16> {
    use crate::daemon::catalog::{self, Binding};
    let mut ports = match entry.binding {
        Binding::Tcp { port } => vec![port],
        Binding::UnixSocket { .. } | Binding::None => Vec::new(),
    };
    // mailpit's web UI isn't expressible as a `Binding`, so add it here —
    // it's a port the service genuinely binds and a common dev collision.
    if entry.name == "mailpit" {
        ports.push(catalog::MAILPIT_HTTP_PORT);
    }
    ports
}

/// From the resolved start order and the set of services our own
/// supervisor already has up, pick the `(name, port)` pairs whose loopback
/// TCP port we should pre-flight probe.
///
/// Skips two cases that can't be real conflicts: services with no TCP port
/// (unix-socket services + runtime-only deps never grab a shared port),
/// and services we already run (re-running `up` is an idempotent no-op, so
/// the port is legitimately ours). A single service may contribute more
/// than one pair (mailpit: SMTP + web UI). Pure + cheap so it's
/// unit-testable without a daemon or a socket.
fn ports_to_preflight(
    order: &[&str],
    active: &std::collections::HashSet<String>,
) -> Vec<(&'static str, u16)> {
    use crate::daemon::catalog;
    order
        .iter()
        .filter_map(|&name| catalog::find(name))
        .filter(|entry| !active.contains(entry.name))
        .flat_map(|entry| {
            service_tcp_ports(entry)
                .into_iter()
                .map(move |port| (entry.name, port))
        })
        .collect()
}

/// Best-effort check whether something already listens on `127.0.0.1:port`
/// (every catalog `Tcp` binding is loopback-only in v1). A successful
/// connect means occupied; any error — refused or our own timeout — is
/// treated as free. Bounded so a black-holed port can't stall the `up`
/// pre-flight.
async fn tcp_port_in_use(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_millis(250),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// Stream a `warning: …` stderr frame for every TCP-bound service whose
/// port is already in use by a foreign process. Advisory only: the start
/// still proceeds into `spawn_service`, whose pre-start bind probe then
/// fails with the authoritative error naming the holder. This early pass
/// exists because it covers *every* requested service (and transitive
/// deps) up front, before the first slow tarball fetch. A disconnected
/// client just drops the warnings — we ignore the write result.
async fn preflight_port_warnings(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    state: &Arc<DaemonState>,
    services: &[ServiceRequest],
) {
    use crate::daemon::supervisor::{self, ServiceState};

    let names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();
    // Resolve transitive deps so a TCP-bound dependency is covered too. If
    // the graph can't resolve (cycle / unknown name), skip the advisory —
    // the real start path reports that error properly.
    let Ok(order) = supervisor::compute_start_order(&names) else {
        return;
    };

    // Services our own supervisor already has up: their port is ours, so a
    // re-run of `up` isn't a conflict. Snapshot once under a brief lock.
    let active: std::collections::HashSet<String> = {
        let sup = state.supervisor.lock().await;
        sup.snapshot()
            .into_iter()
            .filter(|s| {
                matches!(
                    s.state,
                    ServiceState::Running
                        | ServiceState::HealthChecking
                        | ServiceState::Starting
                )
            })
            .map(|s| s.name)
            .collect()
    };

    for (name, port) in ports_to_preflight(&order, &active) {
        if tcp_port_in_use(port).await {
            let msg = format!(
                "warning: 127.0.0.1:{port} is already in use; starting `{name}` may fail \
                 until you free that port\n"
            );
            let _ = write_progress(write_half, "stderr", &msg).await;
        }
    }
}

async fn dispatch_up_streaming(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    state: &Arc<DaemonState>,
    args: ServiceUpArgs,
) {
    use bougie_fetch::DownloadEvent;
    use std::time::Duration;

    // Pre-flight: warn (but don't block) when a TCP-bound service's port
    // is already taken by a process we don't manage. Emitted up-front, as
    // a stderr progress frame the CLI prints verbatim, before the
    // potentially slow tarball fetch + start — so the user sees the likely
    // cause before staring at a health-check timeout.
    preflight_port_warnings(write_half, state, &args.services).await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DownloadEvent>();
    let sink: std::sync::Arc<dyn bougie_fetch::DownloadSink> =
        std::sync::Arc::new(IpcDownloadSink { tx });
    let bar = std::sync::Arc::new(bougie_fetch::DownloadBar::hidden_with_sink(sink));

    let mut up_fut = Box::pin(dispatch_up(state, args.project, args.services, Some(bar.clone())));
    let mut ticker = tokio::time::interval(Duration::from_millis(50));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut pos: u64 = 0;
    let mut total: u64 = 0;
    let mut label = String::new();
    // Mirrors the bar's prefix phase: an `Extracting` event flips it on,
    // the next `Current` (a fresh artifact entering its download) flips
    // it off — exactly how `DownloadBar`'s prefix alternates locally.
    let mut extracting = false;
    let mut dirty = false;
    let mut connected = true;

    let result = loop {
        tokio::select! {
            // Future completes — break with its result; final flush
            // happens after the loop.
            result = &mut up_fut => break result,
            // Aggregate events as they arrive.
            ev = rx.recv() => match ev {
                Some(DownloadEvent::Plan { bytes }) => {
                    total = total.saturating_add(bytes);
                    dirty = true;
                }
                Some(DownloadEvent::Current { name }) => {
                    // A fresh artifact entering its download phase ends
                    // any extraction the prior one was in.
                    if name != label || extracting {
                        label = name;
                        extracting = false;
                        dirty = true;
                    }
                }
                Some(DownloadEvent::Inc { bytes }) => {
                    pos = pos.saturating_add(bytes);
                    dirty = true;
                }
                Some(DownloadEvent::Extracting { name }) => {
                    label = name;
                    extracting = true;
                    dirty = true;
                }
                Some(DownloadEvent::Finish) => {
                    // Per-fetch Finish (one per `ensure_tarball`). The
                    // bar is shared across all services in this `up`,
                    // so we don't tear it down here — the terminal
                    // `result` frame signals "we're done" to the CLI.
                }
                None => {
                    // Sink dropped (shouldn't happen while dispatch_up
                    // is alive — bar holds it). Keep going on the
                    // future itself.
                }
            },
            _ = ticker.tick(), if dirty && connected => {
                if !write_download(write_half, pos, total, &label, extracting).await {
                    connected = false;
                }
                dirty = false;
            }
        }
    };

    // Drain anything still in flight so the bar's final state lands
    // on the wire before the result frame (the CLI will then `finish`
    // its local bar when the terminal arrives).
    while let Ok(ev) = rx.try_recv() {
        match ev {
            DownloadEvent::Plan { bytes } => { total = total.saturating_add(bytes); dirty = true; }
            DownloadEvent::Current { name } => {
                if name != label || extracting { label = name; extracting = false; dirty = true; }
            }
            DownloadEvent::Inc { bytes } => { pos = pos.saturating_add(bytes); dirty = true; }
            DownloadEvent::Extracting { name } => { label = name; extracting = true; dirty = true; }
            DownloadEvent::Finish => {}
        }
    }
    if dirty && connected {
        let _ = write_download(write_half, pos, total, &label, extracting).await;
    }
    write_terminal(write_half, &result).await;
}

/// Streaming `daemon.shutdown`: drain every running service in reverse
/// start-order, emitting one `progress` frame per service so the CLI can
/// report live teardown progress, then a terminal `result` carrying the
/// stopped set. The daemon's shutdown watch channel is signalled *after*
/// the terminal frame is flushed, so the process only tears down — and
/// unlinks its socket — once the client has the full reply. The CLI
/// polls for that socket removal to know the daemon is fully gone.
///
/// The accept-loop's own [`drain`](super::drain) still runs on the way
/// out, but it's an idempotent no-op by then: every service we touched
/// here is already `Stopped`.
///
/// A disconnected client (broken progress/terminal writes) never aborts
/// the drain — the write results are intentionally ignored and the watch
/// channel is signalled regardless, so the daemon always finishes
/// shutting down once asked.
async fn dispatch_shutdown_streaming(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    state: &Arc<DaemonState>,
) {
    use crate::daemon::catalog;
    use crate::daemon::supervisor::ServiceState;

    // Snapshot the running set in reverse start-order. Mirrors
    // `daemon::drain` so the progress we stream matches the teardown
    // the daemon would do anyway.
    let running: Vec<&'static str> = {
        let sup = state.supervisor.lock().await;
        sup.snapshot()
            .into_iter()
            .filter(|s| {
                matches!(
                    s.state,
                    ServiceState::Running
                        | ServiceState::Unhealthy
                        | ServiceState::HealthChecking
                        | ServiceState::Starting
                )
            })
            // Re-resolve to the catalog's 'static name; the snapshot
            // owns a copy as a String.
            .filter_map(|s| catalog::find(&s.name).map(|e| e.name))
            .collect()
    };

    let mut stopped: Vec<String> = Vec::new();
    for &name in running.iter().rev() {
        let _ = write_progress(write_half, "stderr", &format!("stopping {name}\n")).await;
        // Re-take the lock per service: stops are sequential and the
        // grace window per service can be seconds, so holding the lock
        // across the whole drain would needlessly block the status tick.
        let res = state.supervisor.lock().await.stop(name).await;
        match res {
            Ok(true) => stopped.push(name.to_string()),
            // Raced to Stopped/Failed between snapshot and stop — fine.
            Ok(false) => {}
            Err(e) => {
                let _ = write_progress(
                    write_half,
                    "stderr",
                    &format!("warning: stopping {name}: {e}\n"),
                )
                .await;
            }
        }
    }

    let frame = ResultFrame::ok(serde_json::json!({"ok": true, "stopped": stopped}));
    write_terminal(write_half, &frame).await;

    // Drain done and acked — now bring the daemon down. The accept loop
    // wakes from the watch channel and exits; `run()` unlinks the socket
    // + pid file on the way out, which the CLI is polling for.
    let _ = state.shutdown_tx.send(true);
}

/// Build the `BOUGIE_SERVICE_*` env map for this project's tenants.
/// Reads each catalog entry's `tenants.json` and emits per-service
/// vars per SERVICES.md §3.4 (the vocabulary lives in
/// [`tenant_env::tenant_service_env`], shared with `bougie service
/// credentials`). Side-effect free.
async fn dispatch_env(state: &Arc<DaemonState>, project: std::path::PathBuf) -> ResultFrame {
    use crate::daemon::{catalog, tenant_env, tenants};

    let mut vars: serde_json::Map<String, Value> = serde_json::Map::new();
    for entry in catalog::CATALOG {
        if !entry.user_facing {
            continue;
        }
        let tenants_path = state.paths.service_tenants(entry.name);
        let Ok(all) = tenants::load_all(&tenants_path).await else {
            continue;
        };
        let Some(tenant) = all.into_iter().find(|t| t.project == project) else {
            continue;
        };
        vars.extend(tenant_env::tenant_service_env(&state.paths, entry, &tenant));
    }
    ResultFrame::ok(serde_json::json!({"vars": Value::Object(vars)}))
}

async fn dispatch_up(
    state: &Arc<DaemonState>,
    project: std::path::PathBuf,
    services: Vec<ServiceRequest>,
    bar: Option<std::sync::Arc<bougie_fetch::DownloadBar>>,
) -> ResultFrame {
    use crate::daemon::{catalog, provisioners};

    let names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();
    let order = match super::supervisor::compute_start_order(&names) {
        Ok(o) => o,
        Err(e) => return ResultFrame::err("bad_request", e.to_string()),
    };

    let mut started = Vec::new();
    let mut tenants_map = serde_json::Map::new();
    let mut dependencies = serde_json::Map::new();
    for name in order {
        // Skip transitive runtime deps; not all are real services.
        let Some(entry) = catalog::find(name) else { continue };
        // Backstop the tarball: pre_start and supervisor.start both
        // resolve `store_layout::basedir` and bail if it's missing.
        // No-op once the tarball is on disk, so re-runs only pay
        // an `is_dir` check.
        let deps_for_service =
            match super::store_fetch::ensure_tarball(&state.paths, entry, bar.clone()).await {
                Ok(deps) => deps,
                Err(e) => {
                    return ResultFrame::err(
                        "service_tarball_fetch_failed",
                        format!("{name}: {e:#}"),
                    );
                }
            };
        if !deps_for_service.is_empty() {
            // Per UNBUNDLE_PLAN.md Phase 4: only services that
            // actually walked `requires_tools[]` contribute to the
            // inventory. A no-op `ensure_tarball` (outer already on
            // disk) reports nothing.
            if let Ok(v) = serde_json::to_value(&deps_for_service) {
                dependencies.insert(name.to_string(), v);
            }
        }
        // One-shot bootstrap (e.g. mariadb-install-db on first run).
        // Idempotent — safe even when the service is already running.
        // The dispatcher owns the sync/async bridge: mariadb and the
        // other sync provisioners run on the blocking pool internally;
        // opensearch is natively async.
        let pre_res = provisioners::pre_start(entry, &state.paths).await;
        if let Err(e) = pre_res {
            return ResultFrame::err(
                "pre_start_failed",
                format!("{name}: {e}"),
            );
        }
        // Start (idempotent). The health probe runs off the supervisor
        // lock, so a slow service (opensearch/rabbitmq, up to 90s) doesn't
        // block `status` or the reaper while we wait for it to come up.
        let start_res = super::supervisor::start_service(&state.supervisor, name).await;
        match start_res {
            Ok(true) => started.push(name.to_string()),
            Ok(false) => {}
            Err(e) => {
                return ResultFrame::err(
                    "service_start_failed",
                    format!("{name}: {e}"),
                );
            }
        }
        // Provision the tenant for this project (only for user_facing
        // entries — runtime deps have no tenancy).
        if entry.user_facing {
            let tenant_name = match services.iter().find(|s| s.name == name) {
                Some(s) => s.tenant.clone(),
                None => continue, // dep ordered in but not in the request
            };
            let tenants_path = state.paths.service_tenants(name);
            let prov_res = provisioners::provision(
                entry,
                &state.paths,
                &tenants_path,
                &tenant_name,
                &project,
            )
            .await;
            match prov_res {
                Ok(t) => {
                    tenants_map.insert(name.to_string(), Value::String(t.tenant));
                }
                Err(e) => {
                    return ResultFrame::err(
                        "provision_failed",
                        format!("{name}: {e:#}"),
                    );
                }
            }
        }
    }

    ResultFrame::ok(serde_json::json!({
        "started": started,
        "tenants": Value::Object(tenants_map),
        "dependencies": Value::Object(dependencies),
    }))
}

async fn dispatch_down(
    state: &Arc<DaemonState>,
    project: std::path::PathBuf,
    services: Vec<String>,
    purge: bool,
) -> ResultFrame {
    use crate::daemon::{catalog, provisioners, tenants};

    let mut stopped = Vec::new();
    let mut deprovisioned = Vec::new();
    for name in services {
        let Some(entry) = catalog::find(&name) else {
            return ResultFrame::err("unknown_service", format!("`{name}` not in catalog"));
        };
        if entry.user_facing {
            let tenants_path = state.paths.service_tenants(entry.name);
            // Find this project's tenant; if any, deprovision it.
            let project_tenant = tenants::load_all(&tenants_path)
                .await
                .ok()
                .and_then(|all| all.into_iter().find(|t| t.project == project));
            if let Some(t) = project_tenant {
                let sock_default = state.paths.service_run(entry.name).join(format!("{}.sock", entry.name));
                let sock_opt = sock_default.exists().then_some(sock_default);
                let deprov_res = provisioners::deprovision(
                    entry,
                    &state.paths,
                    &tenants_path,
                    &t.tenant,
                    sock_opt.as_deref(),
                    purge,
                )
                .await;
                if let Err(e) = deprov_res {
                    return ResultFrame::err(
                        "deprovision_failed",
                        format!("{}: {:#}", entry.name, e),
                    );
                }
                deprovisioned.push(entry.name.to_string());
            }
            // Stop the global service iff no tenants remain.
            let remaining = tenants::load_all(&tenants_path)
                .await
                .map_or(0, |v| v.len());
            if remaining == 0 {
                let stop_res = state.supervisor.lock().await.stop(entry.name).await;
                if let Ok(true) = stop_res {
                    stopped.push(entry.name.to_string());
                }
            }
        }
    }
    ResultFrame::ok(serde_json::json!({
        "stopped": stopped,
        "deprovisioned": deprovisioned,
    }))
}

// Watch channel type re-exported for `DaemonState`.
pub(super) type ShutdownTx = watch::Sender<bool>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn preflight_skips_unix_socket_and_runtime_dep_services() {
        // redis/mariadb bind unix sockets; jdk/erlang are runtime-only
        // deps with no binding — none of them grab a shared TCP port.
        let active = HashSet::new();
        let order = ["redis", "mariadb", "jdk", "erlang"];
        assert!(ports_to_preflight(&order, &active).is_empty());
    }

    #[test]
    fn preflight_includes_tcp_services_not_yet_active() {
        let active = HashSet::new();
        let order = ["opensearch", "rabbitmq", "server"];
        let got = ports_to_preflight(&order, &active);
        assert!(got.contains(&("opensearch", 9200)), "{got:?}");
        assert!(got.contains(&("rabbitmq", 5672)), "{got:?}");
        assert!(got.contains(&("server", 7080)), "{got:?}");
    }

    #[test]
    fn preflight_skips_services_we_already_run() {
        // opensearch is already up under our own supervisor: its port is
        // legitimately ours, so a re-run of `up` isn't a conflict.
        let active: HashSet<String> = ["opensearch".to_string()].into_iter().collect();
        let order = ["opensearch", "rabbitmq"];
        assert_eq!(ports_to_preflight(&order, &active), vec![("rabbitmq", 5672)]);
    }

    #[test]
    fn preflight_covers_both_mailpit_ports() {
        use crate::daemon::catalog::{MAILPIT_HTTP_PORT, MAILPIT_SMTP_PORT};
        // mailpit binds SMTP (its catalog `Binding`) *and* a web UI that
        // the single-port binding can't express — pre-flight must probe both.
        let active = HashSet::new();
        let got = ports_to_preflight(&["mailpit"], &active);
        assert!(got.contains(&("mailpit", MAILPIT_SMTP_PORT)), "{got:?}");
        assert!(got.contains(&("mailpit", MAILPIT_HTTP_PORT)), "{got:?}");
        assert_eq!(got.len(), 2, "exactly SMTP + web UI: {got:?}");
    }

    #[test]
    fn preflight_skips_both_mailpit_ports_when_already_running() {
        let active: HashSet<String> = ["mailpit".to_string()].into_iter().collect();
        assert!(ports_to_preflight(&["mailpit"], &active).is_empty());
    }

    #[test]
    fn parses_status_request() {
        let r = parse_request(r#"{"v": 1, "method": "status"}"#).unwrap();
        assert!(matches!(r, Request::Status));
    }

    #[test]
    fn parses_daemon_version_request() {
        let r = parse_request(r#"{"v": 1, "method": "daemon.version"}"#).unwrap();
        assert!(matches!(r, Request::DaemonVersion));
    }

    #[test]
    fn parses_daemon_shutdown_request() {
        let r = parse_request(r#"{"v": 1, "method": "daemon.shutdown"}"#).unwrap();
        assert!(matches!(r, Request::DaemonShutdown));
    }

    #[test]
    fn rejects_unknown_wire_version() {
        let err = parse_request(r#"{"v": 2, "method": "status"}"#).unwrap_err();
        assert!(err.contains("v=2"), "{err}");
    }

    #[test]
    fn rejects_unknown_method() {
        let err = parse_request(r#"{"v": 1, "method": "bogus"}"#).unwrap_err();
        assert!(err.contains("unknown method"), "{err}");
    }

    #[test]
    fn parses_service_up_request() {
        let r = parse_request(
            r#"{"v": 1, "method": "service.up", "args": {"project": "/p", "services": [{"name": "redis", "tenant": "acme"}]}}"#,
        )
        .unwrap();
        let Request::ServiceUp(args) = r else {
            panic!("expected ServiceUp");
        };
        assert_eq!(args.project, std::path::Path::new("/p"));
        assert_eq!(args.services.len(), 1);
        assert_eq!(args.services[0].name, "redis");
        assert_eq!(args.services[0].tenant, "acme");
    }

    #[test]
    fn parses_service_restart_request() {
        let r = parse_request(
            r#"{"v": 1, "method": "service.restart", "args": {"project": "/p", "services": ["redis", "mariadb"]}}"#,
        )
        .unwrap();
        let Request::ServiceRestart(args) = r else {
            panic!("expected ServiceRestart");
        };
        assert_eq!(args.project, std::path::Path::new("/p"));
        assert_eq!(args.services, vec!["redis".to_string(), "mariadb".to_string()]);
    }

    #[test]
    fn parses_catalog_request() {
        let r = parse_request(r#"{"v": 1, "method": "catalog"}"#).unwrap();
        assert!(matches!(r, Request::Catalog));
    }

    #[test]
    fn dispatch_catalog_returns_known_service_names() {
        let frame = dispatch_catalog();
        assert!(frame.ok);
        let val = frame.result.unwrap();
        let entries = val["catalog"].as_array().expect("catalog array");
        let names: Vec<&str> = entries
            .iter()
            .filter_map(|e| e["name"].as_str())
            .collect();
        assert!(names.contains(&"redis"), "{names:?}");
        assert!(names.contains(&"mariadb"), "{names:?}");
        assert!(names.contains(&"rabbitmq"), "{names:?}");
    }

    #[test]
    fn parses_service_down_request_with_purge() {
        let r = parse_request(
            r#"{"v": 1, "method": "service.down", "args": {"project": "/p", "services": ["redis"], "purge": true}}"#,
        )
        .unwrap();
        let Request::ServiceDown(args) = r else {
            panic!("expected ServiceDown");
        };
        assert!(args.purge);
    }

    #[test]
    fn ok_frame_serializes_without_error_field() {
        let f = ResultFrame::ok(serde_json::json!({"a": 1}));
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains(r#""schema_version":1"#));
        assert!(s.contains(r#""type":"result""#));
        assert!(s.contains(r#""ok":true"#));
        assert!(s.contains(r#""result":{"a":1}"#));
        assert!(!s.contains(r#""error":"#), "{s}");
    }

    #[test]
    fn err_frame_serializes_without_result_field() {
        let f = ResultFrame::err("redis_db_exhausted", "all 16 redis DB numbers in use");
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains(r#""ok":false"#));
        assert!(s.contains(r#""code":"redis_db_exhausted""#));
        // Check for the FIELD "result", not the literal `"result"`
        // which also appears as the value of `"type"`.
        assert!(!s.contains(r#""result":"#), "{s}");
    }

    #[test]
    fn progress_frame_serializes() {
        let f = ProgressFrame::new("stderr", "downloading redis\n");
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains(r#""type":"progress""#));
        assert!(s.contains(r#""stream":"stderr""#));
        assert!(s.contains("downloading redis"));
    }

    #[test]
    fn download_frame_carries_extracting_phase() {
        // The extracting flag rides on every download frame so the CLI's
        // mirrored bar can flip its prefix; assert both phases serialize.
        let dl = DownloadFrame::new(40, 100, "opensearch-2.19.5", false);
        let s = serde_json::to_string(&dl).unwrap();
        assert!(s.contains(r#""type":"download""#));
        assert!(s.contains(r#""extracting":false"#));

        let ex = DownloadFrame::new(100, 100, "jdk-21.0.11_10", true);
        let s = serde_json::to_string(&ex).unwrap();
        assert!(s.contains(r#""extracting":true"#));
    }

    #[test]
    fn service_logs_accepts_single_service_form() {
        let r = parse_request(
            r#"{"v": 1, "method": "service.logs", "args": {"service": "redis", "follow": true}}"#,
        )
        .unwrap();
        let Request::ServiceLogs(args) = r else {
            panic!("expected ServiceLogs");
        };
        assert_eq!(args.service_names(), vec!["redis".to_string()]);
        assert!(args.follow);
        // `lines` falls back to the default when omitted.
        assert_eq!(args.lines, default_lines());
    }

    #[test]
    fn service_logs_accepts_multi_service_form() {
        let r = parse_request(
            r#"{"v": 1, "method": "service.logs", "args": {"services": ["redis", "mariadb"], "lines": 10}}"#,
        )
        .unwrap();
        let Request::ServiceLogs(args) = r else {
            panic!("expected ServiceLogs");
        };
        assert_eq!(args.service_names(), vec!["redis".to_string(), "mariadb".to_string()]);
        assert_eq!(args.lines, 10);
    }

    #[test]
    fn service_logs_prefers_services_array_over_lone_service() {
        // Defensive: if a caller sends both, the explicit array wins.
        let args = ServiceLogsArgs {
            service: Some("redis".into()),
            services: vec!["mariadb".into()],
            lines: 50,
            follow: false,
            color: false,
            host: None,
        };
        assert_eq!(args.service_names(), vec!["mariadb".to_string()]);
    }

    #[test]
    fn drain_keeps_only_matching_complete_lines() {
        let mut p = b"a shop.bougie.run x\nb other.bougie.run y\nc shop.bougie.run z\ntrailing"
            .to_vec();
        let out = drain_matching_lines(&mut p, Some("shop.bougie.run"));
        assert_eq!(out, "a shop.bougie.run x\nc shop.bougie.run z\n");
        // The partial (newline-less) tail is retained for the next read.
        assert_eq!(p, b"trailing");
    }

    #[test]
    fn drain_without_filter_passes_every_complete_line() {
        let mut p = b"one\ntwo\nthr".to_vec();
        let out = drain_matching_lines(&mut p, None);
        assert_eq!(out, "one\ntwo\n");
        assert_eq!(p, b"thr");
    }

    #[test]
    fn service_logs_parses_host_filter() {
        let r = parse_request(
            r#"{"v": 1, "method": "service.logs", "args": {"service": "server", "host": "shop.bougie.run"}}"#,
        )
        .unwrap();
        let Request::ServiceLogs(args) = r else {
            panic!("expected ServiceLogs");
        };
        assert_eq!(args.host.as_deref(), Some("shop.bougie.run"));
    }

    #[test]
    fn prefix_line_pads_name_to_width() {
        let mut out = String::new();
        prefix_line(&mut out, "redis", 7, None, "ready to accept connections\n");
        prefix_line(&mut out, "mariadb", 7, None, "starting\n");
        assert_eq!(
            out,
            "redis   | ready to accept connections\nmariadb | starting\n"
        );
    }

    #[test]
    fn prefix_line_colors_only_the_prefix() {
        let mut out = String::new();
        prefix_line(&mut out, "redis", 7, Some(36), "ready\n");
        // Cyan (36) opens, resets before the log text, which stays plain.
        assert_eq!(out, "\x1b[36mredis   |\x1b[0m ready\n");
    }

    #[tokio::test]
    async fn reopen_if_rotated_follows_across_rotation() {
        // Reproduces the `logs --follow` silence: after LogWriter renames
        // svc.log → svc.log.1 and opens a fresh svc.log, the follower's fd
        // stays on the renamed inode (endless EOF). reopen_if_rotated must
        // detect the inode change and hand back the fresh file so following
        // resumes from the rotated-in content.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("svc.log");
        std::fs::write(&path, b"one\n").unwrap();
        let f = tokio::fs::OpenOptions::new().read(true).open(&path).await.unwrap();

        // Same inode, just idle → no reopen.
        assert!(reopen_if_rotated(&path, &f).await.is_none());

        // Rotate: rename the current file away, create a fresh one.
        std::fs::rename(&path, dir.path().join("svc.log.1")).unwrap();
        std::fs::write(&path, b"two\n").unwrap();

        // Inode changed → reopen, and the new handle reads from the start.
        let reopened = reopen_if_rotated(&path, &f).await;
        let mut nf = reopened.expect("rotation must trigger a reopen");
        let mut s = String::new();
        nf.read_to_string(&mut s).await.unwrap();
        assert_eq!(s, "two\n", "the reopened handle follows the new file");
    }
}
