//! Line-delimited JSON IPC for `bougied`.
//!
//! Wire format mirrors the existing `bougie server` control socket
//! (`src/commands/server/control.rs`) so the two daemons feel
//! consistent to operators:
//!
//! Request:  `{"v": 1, "method": "<name>", "args": {...}}\n`
//!
//! Response: zero or more `progress` frames followed by exactly one
//! `result` frame; both `\n`-terminated.
//!
//! ```jsonc
//! {"schema_version": 1, "type": "progress", "stream": "stderr", "data": "…"}
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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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
    /// `bougie services catalog` shows locally; exposed via IPC for
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
    pub service: String,
    #[serde(default = "default_lines")]
    pub lines: usize,
    #[serde(default)]
    pub follow: bool,
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
        // service.logs is the only streaming method today: it writes
        // its own progress frames + a terminal frame (or never sends a
        // terminal in follow-mode, in which case the client closes the
        // connection).
        Ok(Request::ServiceLogs(args)) => {
            dispatch_logs(&mut write_half, &state, args).await;
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
        Request::DaemonShutdown => {
            // Tell the main loop to drain. The accept loop wakes
            // from the watch channel and exits; we get a clean
            // socket-removal in the drop path of `DaemonState`.
            let _ = state.shutdown_tx.send(true);
            ResultFrame::ok(serde_json::json!({"ok": true}))
        }
        Request::ServiceUp(args) => dispatch_up(state, args.project, args.services).await,
        Request::ServiceDown(args) => {
            dispatch_down(state, args.project, args.services, args.purge).await
        }
        Request::ServiceRestart(args) => dispatch_restart(state, args.services).await,
        Request::ServiceEnv(args) => dispatch_env(state, args.project).await,
        Request::ServiceLogs(_) => unreachable!("handled in handle_connection"),
        Request::Catalog => dispatch_catalog(),
    }
}

/// Render the in-binary catalog as a JSON value.
///
/// The CLI's `bougie services catalog` reads `catalog::CATALOG`
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
async fn dispatch_restart(
    state: &Arc<DaemonState>,
    services: Vec<String>,
) -> ResultFrame {
    use crate::daemon::catalog;
    let names: Vec<&str> = services.iter().map(|s| s.as_str()).collect();
    // Re-order to respect after/requires graph. Same topology used
    // by `service.up` so a `restart` of dependents lines up the same
    // way as a fresh boot.
    let order = match super::supervisor::compute_start_order(&names) {
        Ok(o) => o,
        Err(e) => return ResultFrame::err("bad_request", e.to_string()),
    };
    let mut restarted = Vec::new();
    for name in order {
        // Skip transitively-pulled runtime deps that aren't real
        // managed processes (jdk, erlang).
        if !catalog::find(name).map(|e| e.user_facing).unwrap_or(false) {
            continue;
        }
        let mut sup = state.supervisor.lock().await;
        // `stop` returns Ok(false) when the service wasn't running;
        // skip those — `restart` of a stopped service is a no-op,
        // matching `systemctl restart` semantics.
        let was_running = match sup.stop(name).await {
            Ok(true) => true,
            Ok(false) => false,
            Err(e) => {
                return ResultFrame::err(
                    "service_stop_failed",
                    format!("{}: {}", name, e),
                );
            }
        };
        if was_running {
            if let Err(e) = sup.start(name).await {
                return ResultFrame::err(
                    "service_start_failed",
                    format!("{}: {}", name, e),
                );
            }
            restarted.push(name.to_string());
        }
    }
    ResultFrame::ok(serde_json::json!({"restarted": restarted}))
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
    use crate::daemon::{catalog, logs};

    let Some(entry) = catalog::find(&args.service) else {
        let frame = ResultFrame::err("unknown_service", format!("`{}` not in catalog", args.service));
        write_terminal(write_half, &frame).await;
        return;
    };
    let log_path = state
        .paths
        .service_log(entry.name)
        .join(format!("{}.log", entry.name));

    // 1. Tail.
    let tail = logs::tail_lines(&log_path, args.lines).unwrap_or_default();
    let joined = tail.concat();
    if !joined.is_empty() {
        if !write_progress(write_half, "stdout", &joined).await {
            return;
        }
    }

    if !args.follow {
        let frame = ResultFrame::ok(serde_json::json!({"lines_tailed": tail.len()}));
        write_terminal(write_half, &frame).await;
        return;
    }

    // 2. Follow: seek to current end-of-file, poll for growth, stream
    // new bytes. Buffer reused across iterations to avoid alloc churn.
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
    let mut f = match tokio::fs::OpenOptions::new().read(true).open(&log_path).await {
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
    loop {
        match f.read(&mut buf).await {
            Ok(0) => {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]);
                if !write_progress(write_half, "stdout", &chunk).await {
                    return;
                }
            }
            Err(_) => return,
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

/// Percent-encode the AMQP-DSN-significant characters. Tenant names
/// and passwords today are constrained to `[a-z0-9_]+` and hex
/// respectively, so the encoder is a no-op on the happy path; it's
/// defence-in-depth against a future widening of those validators.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build the `BOUGIE_SERVICE_*` env map for this project's tenants.
/// Reads each catalog entry's `tenants.json` and emits per-service
/// vars per SERVICES.md §3.4. Side-effect free.
async fn dispatch_env(state: &Arc<DaemonState>, project: std::path::PathBuf) -> ResultFrame {
    use crate::daemon::{catalog, tenants};

    let mut vars: serde_json::Map<String, Value> = serde_json::Map::new();
    for entry in catalog::CATALOG {
        if !entry.user_facing {
            continue;
        }
        let tenants_path = state.paths.service_tenants(entry.name);
        let Ok(all) = tenants::load_all(&tenants_path) else {
            continue;
        };
        let Some(tenant) = all.into_iter().find(|t| t.project == project) else {
            continue;
        };
        let prefix = format!("BOUGIE_SERVICE_{}_", entry.name.to_ascii_uppercase());
        match entry.name {
            "redis" => {
                let sock = state
                    .paths
                    .service_run("redis")
                    .join("redis.sock")
                    .display()
                    .to_string();
                vars.insert(format!("{prefix}SOCKET"), Value::String(sock));
                if let Some(db) = tenant.alloc.get("db_number") {
                    vars.insert(format!("{prefix}DB"), db.clone());
                }
            }
            "mariadb" => {
                let sock = state
                    .paths
                    .service_run("mariadb")
                    .join("mariadb.sock")
                    .display()
                    .to_string();
                vars.insert(format!("{prefix}SOCKET"), Value::String(sock));
                vars.insert(
                    format!("{prefix}DATABASE"),
                    Value::String(tenant.tenant.clone()),
                );
                vars.insert(
                    format!("{prefix}USER"),
                    Value::String(tenant.tenant.clone()),
                );
                if let Some(pw) = tenant.secrets.get("password") {
                    vars.insert(format!("{prefix}PASSWORD"), Value::String(pw.clone()));
                }
            }
            "opensearch" => {
                // Catalog binding pins :9200 (loopback only). Surface
                // both the base URL and the tenant's reserved index
                // prefix so apps build `<prefix>articles` etc.
                vars.insert(
                    format!("{prefix}URL"),
                    Value::String("http://127.0.0.1:9200".into()),
                );
                if let Some(p) = tenant.alloc.get("index_prefix") {
                    vars.insert(format!("{prefix}INDEX_PREFIX"), p.clone());
                }
            }
            "server" => {
                // Catalog binding pins 127.0.0.1:7080. Surface the
                // root URL alongside the tenant's reserved hostname
                // so apps can build absolute redirects without
                // re-encoding the suffix.
                vars.insert(
                    format!("{prefix}URL"),
                    Value::String("http://127.0.0.1:7080".into()),
                );
                if let Some(h) = tenant.alloc.get("hostname") {
                    vars.insert(format!("{prefix}HOSTNAME"), h.clone());
                }
            }
            "rabbitmq" => {
                // Catalog binding pins 127.0.0.1:5672. Compose the
                // full AMQP DSN so apps don't have to assemble the
                // pieces; vhost lives in the path component, user
                // and password in the authority.
                let user = tenant
                    .alloc
                    .get("username")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&tenant.tenant);
                let vhost = tenant
                    .alloc
                    .get("vhost")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&tenant.tenant);
                let pw = tenant.secrets.get("password").cloned().unwrap_or_default();
                let url = format!(
                    "amqp://{}:{}@127.0.0.1:5672/{}",
                    urlencode(user),
                    urlencode(&pw),
                    urlencode(vhost),
                );
                vars.insert(format!("{prefix}URL"), Value::String(url));
                vars.insert(format!("{prefix}VHOST"), Value::String(vhost.to_string()));
                vars.insert(format!("{prefix}USER"), Value::String(user.to_string()));
                if !pw.is_empty() {
                    vars.insert(format!("{prefix}PASSWORD"), Value::String(pw));
                }
            }
            _ => {}
        }
    }
    ResultFrame::ok(serde_json::json!({"vars": Value::Object(vars)}))
}

async fn dispatch_up(
    state: &Arc<DaemonState>,
    project: std::path::PathBuf,
    services: Vec<ServiceRequest>,
) -> ResultFrame {
    use crate::daemon::{catalog, provisioners};

    let names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();
    let order = match super::supervisor::compute_start_order(&names) {
        Ok(o) => o,
        Err(e) => return ResultFrame::err("bad_request", e.to_string()),
    };

    let mut started = Vec::new();
    let mut tenants_map = serde_json::Map::new();
    for name in order {
        // Skip transitive runtime deps; not all are real services.
        let Some(entry) = catalog::find(name) else { continue };
        // Backstop the tarball: pre_start and supervisor.start both
        // resolve `store_layout::basedir` and bail if it's missing.
        // No-op once the tarball is on disk, so re-runs only pay
        // an `is_dir` check.
        if let Err(e) = super::store_fetch::ensure_tarball(&state.paths, entry).await {
            return ResultFrame::err(
                "service_tarball_fetch_failed",
                format!("{}: {:#}", name, e),
            );
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
                format!("{}: {}", name, e),
            );
        }
        // Start (idempotent).
        let start_res = state.supervisor.lock().await.start(name).await;
        match start_res {
            Ok(true) => started.push(name.to_string()),
            Ok(false) => {}
            Err(e) => {
                return ResultFrame::err(
                    "service_start_failed",
                    format!("{}: {}", name, e),
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
                        format!("{}: {:#}", name, e),
                    );
                }
            }
        }
    }

    ResultFrame::ok(serde_json::json!({
        "started": started,
        "tenants": Value::Object(tenants_map),
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
                .map(|v| v.len())
                .unwrap_or(0);
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

// Watch channel type re-exported for `DaemonState`.
pub(super) type ShutdownTx = watch::Sender<bool>;
