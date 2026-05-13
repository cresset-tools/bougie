//! HTTP request dispatch. Phase-1 surface:
//!
//! 1. Match `Host:` against the configured host list (incl. aliases).
//! 2. Run `try_files` (`static_files::resolve`).
//! 3. Serve the resolved file with `Cache-Control: no-cache` and a
//!    mime-guessed `Content-Type`. `.php` matches return 501 with a
//!    pointer to phase 2 until the `FastCGI` dispatcher lands.

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncReadExt;

use super::config::{HostBlock, ServerConfig};
use super::log::{emit_request, RequestRow};
use super::static_files::{self, Resolution};

#[derive(Debug)]
pub struct AppState {
    /// Maps every configured hostname (canonical + aliases) to its
    /// `[[host]]` block. Populated once at startup.
    pub hosts: HashMap<String, Arc<HostBlock>>,
}

impl AppState {
    pub fn from_config(config: &ServerConfig) -> eyre::Result<Self> {
        let mut hosts = HashMap::new();
        for block in &config.hosts {
            let shared = Arc::new(block.clone());
            insert_unique(&mut hosts, &block.hostname, &shared)?;
            for alias in &block.aliases {
                insert_unique(&mut hosts, &alias.hostname, &shared)?;
            }
        }
        Ok(Self { hosts })
    }
}

fn insert_unique(
    map: &mut HashMap<String, Arc<HostBlock>>,
    name: &str,
    block: &Arc<HostBlock>,
) -> eyre::Result<()> {
    if map.contains_key(name) {
        return Err(eyre::eyre!("duplicate hostname in server.toml: {name}"));
    }
    map.insert(name.to_ascii_lowercase(), Arc::clone(block));
    Ok(())
}

pub fn build(state: Arc<AppState>) -> Router {
    Router::new().fallback(any(dispatch)).with_state(state)
}

async fn dispatch(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();
    let host_header = host_from_headers(req.headers()).unwrap_or_default();
    let host_key = host_header.to_ascii_lowercase();

    let resp = match state.hosts.get(&host_key) {
        None => unknown_host(&host_header),
        Some(host) => serve(host, &path, &query).await,
    };

    let (status, bytes_out) = response_summary(&resp);
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let row = RequestRow::new(
        method.as_str(),
        &host_header,
        &path,
        status,
        bytes_out,
        duration_ms,
    );
    emit_request(&row);

    resp
}

/// Extract the bare hostname (no port) from the Host header. Falls back
/// to an empty string when absent — that lands on the "unknown host"
/// branch the same as a hostname not in the config.
fn host_from_headers(h: &HeaderMap) -> Option<String> {
    let raw = h.get(header::HOST)?.to_str().ok()?;
    Some(strip_port(raw).to_owned())
}

fn strip_port(host: &str) -> &str {
    // IPv6 bracketed: `[::1]:7080` → `[::1]`. We strip the trailing
    // `:port` for non-bracketed; for bracketed we keep the brackets.
    if let Some(rest) = host.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return &host[..=end + 1];
    }
    host.rsplit_once(':').map_or(host, |(name, _)| name)
}

async fn serve(host: &Arc<HostBlock>, path: &str, query: &str) -> Response {
    match static_files::resolve(host, path, query) {
        Resolution::Static { path: file } => serve_static(&file).await,
        Resolution::Php { .. } => php_not_yet_implemented(),
        Resolution::NotFound => not_found(),
        Resolution::Forbidden => forbidden(),
    }
}

async fn serve_static(file: &std::path::Path) -> Response {
    let Ok(mut fh) = tokio::fs::File::open(file).await else {
        return not_found();
    };
    let Ok(meta) = fh.metadata().await else {
        return not_found();
    };
    let len = meta.len();
    let cap = usize::try_from(len.min(8 * 1024 * 1024)).unwrap_or(0);
    let mut buf = Vec::with_capacity(cap);
    if fh.read_to_end(&mut buf).await.is_err() {
        return internal_error();
    }

    let mime = static_files::mime_for(file);
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONTENT_LENGTH, buf.len())
        .body(Body::from(buf))
        .expect("static response is well-formed");
    // Echo a debug header so users can see what was matched in the
    // browser. Phase 2 will replace this for .php dispatch with
    // `X-Bougie-Pool: normal|xdebug` per the spec.
    if let Ok(v) = HeaderValue::from_str(mime) {
        resp.headers_mut().insert("x-bougie-static-mime", v);
    }
    resp
}

fn unknown_host(host: &str) -> Response {
    let body = format!("bougie: unknown host {host}\n");
    plain_response(StatusCode::NOT_FOUND, body)
}

fn not_found() -> Response {
    plain_response(StatusCode::NOT_FOUND, "bougie: file not found\n".into())
}

fn forbidden() -> Response {
    plain_response(StatusCode::FORBIDDEN, "bougie: forbidden\n".into())
}

fn internal_error() -> Response {
    plain_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "bougie: internal error\n".into(),
    )
}

fn php_not_yet_implemented() -> Response {
    plain_response(
        StatusCode::NOT_IMPLEMENTED,
        "bougie: PHP dispatch lands in phase 2 (FastCGI)\n".into(),
    )
}

fn plain_response(status: StatusCode, body: String) -> Response {
    let len = body.len();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CONTENT_LENGTH, len)
        .body(Body::from(body))
        .expect("plain response is well-formed")
}

fn response_summary(resp: &Response) -> (u16, u64) {
    let status = resp.status().as_u16();
    let bytes = resp
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    (status, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::server::config::HostAlias;

    fn block(host: &str, aliases: &[&str]) -> HostBlock {
        HostBlock {
            hostname: host.into(),
            project: std::path::PathBuf::from("/tmp"),
            root: ".".into(),
            index: Vec::new(),
            try_files: Vec::new(),
            aliases: aliases.iter().map(|a| HostAlias { hostname: (*a).to_string() }).collect(),
        }
    }

    #[test]
    fn app_state_indexes_canonical_and_alias() {
        let cfg = ServerConfig {
            server: super::super::config::ServerSection::default(),
            hosts: vec![block("a.bougie.run", &["b.bougie.run"])],
        };
        let state = AppState::from_config(&cfg).unwrap();
        assert!(state.hosts.contains_key("a.bougie.run"));
        assert!(state.hosts.contains_key("b.bougie.run"));
    }

    #[test]
    fn duplicate_hostname_errors_at_build() {
        let cfg = ServerConfig {
            server: super::super::config::ServerSection::default(),
            hosts: vec![block("dup.bougie.run", &[]), block("dup.bougie.run", &[])],
        };
        assert!(AppState::from_config(&cfg).is_err());
    }

    #[test]
    fn alias_collision_errors() {
        let cfg = ServerConfig {
            server: super::super::config::ServerSection::default(),
            hosts: vec![
                block("a.bougie.run", &["shared.bougie.run"]),
                block("b.bougie.run", &["shared.bougie.run"]),
            ],
        };
        assert!(AppState::from_config(&cfg).is_err());
    }

    #[test]
    fn host_lookup_is_case_insensitive() {
        let cfg = ServerConfig {
            server: super::super::config::ServerSection::default(),
            hosts: vec![block("Case.Bougie.Run", &[])],
        };
        let state = AppState::from_config(&cfg).unwrap();
        assert!(state.hosts.contains_key("case.bougie.run"));
    }

    #[test]
    fn strip_port_handles_ipv4() {
        assert_eq!(strip_port("foo.bougie.run:7080"), "foo.bougie.run");
        assert_eq!(strip_port("foo.bougie.run"), "foo.bougie.run");
    }

    #[test]
    fn strip_port_keeps_ipv6_brackets() {
        assert_eq!(strip_port("[::1]:7080"), "[::1]");
        assert_eq!(strip_port("[::1]"), "[::1]");
    }
}
