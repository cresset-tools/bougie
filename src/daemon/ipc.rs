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
/// `DaemonShutdown` carry no args; `ServiceUp` / `ServiceDown` pull
/// their fields out of the envelope's `args` object.
#[derive(Debug)]
pub enum Request {
    Status,
    DaemonVersion,
    DaemonShutdown,
    ServiceUp(ServiceUpArgs),
    ServiceDown(ServiceDownArgs),
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

    let frame = match parse_request(line.trim()) {
        Ok(req) => dispatch(req, &state).await,
        Err(e) => ResultFrame::err("bad_request", e),
    };

    let bytes = match serde_json::to_vec(&frame) {
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
            "version": env!("CARGO_PKG_VERSION"),
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
    }
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
            match provisioners::provision(entry, &tenants_path, &tenant_name, &project) {
                Ok(t) => {
                    tenants_map.insert(name.to_string(), Value::String(t.tenant));
                }
                Err(e) => {
                    return ResultFrame::err(
                        "provision_failed",
                        format!("{}: {}", name, e),
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
                let sock = if sock_default.exists() { Some(sock_default.as_path()) } else { None };
                if let Err(e) = provisioners::deprovision(entry, &tenants_path, &t.tenant, sock, purge) {
                    return ResultFrame::err(
                        "deprovision_failed",
                        format!("{}: {}", entry.name, e),
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
