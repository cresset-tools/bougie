//! HTTP request dispatch. Phase-2 surface:
//!
//! 1. Match `Host:` against the configured host list (incl. aliases).
//! 2. Run `try_files` (`static_files::resolve`).
//! 3. Serve the resolved file with `Cache-Control: no-cache` and a
//!    mime-guessed `Content-Type`. `.php` matches dispatch to a
//!    php-fpm pool over FastCGI via [`super::pool`].
//!
//! Phase 3 layers per-request xdebug routing on top of the
//! `variant="normal"` plumbing this module installs.

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use http_body_util::BodyExt;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncReadExt;

use super::config::{HostBlock, ServerConfig};
use super::fastcgi;
use super::log::{emit_request, RequestRow};
use super::pool::PoolManager;
use super::static_files::{self, Resolution};

/// Upper bound on request body bytes forwarded to FastCGI. 32 MB
/// matches SERVER_PLAN.md's deferred decision; the cap can be lifted
/// to a config knob if anyone hits it.
const MAX_REQUEST_BODY: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub struct AppState {
    /// Maps every configured hostname (canonical + aliases) to its
    /// `[[host]]` block. Populated once at startup.
    pub hosts: HashMap<String, Arc<HostBlock>>,
    pub pools: Arc<PoolManager>,
    pub listen_port: u16,
}

impl AppState {
    pub fn build(
        config: &ServerConfig,
        pools: Arc<PoolManager>,
        listen_port: u16,
    ) -> eyre::Result<Self> {
        let mut hosts = HashMap::new();
        for block in &config.hosts {
            let shared = Arc::new(block.clone());
            insert_unique(&mut hosts, &block.hostname, &shared)?;
            for alias in &block.aliases {
                insert_unique(&mut hosts, &alias.hostname, &shared)?;
            }
        }
        Ok(Self { hosts, pools, listen_port })
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

async fn dispatch(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();
    let host_header = host_from_headers(req.headers()).unwrap_or_default();
    let host_key = host_header.to_ascii_lowercase();

    let resp = match state.hosts.get(&host_key) {
        None => unknown_host(&host_header),
        Some(host) => serve(&state, host, &host_header, &path, &query, peer, req).await,
    };

    let (status, bytes_out) = response_summary(&resp);
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut row = RequestRow::new(
        method.as_str(),
        &host_header,
        &path,
        status,
        bytes_out,
        duration_ms,
    );
    // Surface the pool flavour (currently always "normal" in phase 2)
    // on the log row so phase 3 doesn't need to re-thread it.
    if let Some(pool) = resp
        .headers()
        .get("x-bougie-pool")
        .and_then(|v| v.to_str().ok())
    {
        row = row.with_pool(pool);
    }
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

async fn serve(
    state: &Arc<AppState>,
    host: &Arc<HostBlock>,
    host_header: &str,
    path: &str,
    query: &str,
    peer: SocketAddr,
    req: Request<Body>,
) -> Response {
    match static_files::resolve(host, path, query) {
        Resolution::Static { path: file } => serve_static(&file).await,
        Resolution::Php { script_filename, script_name, path_info } => {
            serve_php(
                state,
                host,
                host_header,
                &script_filename,
                &script_name,
                &path_info,
                query,
                peer,
                req,
            )
            .await
        }
        Resolution::NotFound => not_found(),
        Resolution::Forbidden => forbidden(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve_php(
    state: &Arc<AppState>,
    host: &Arc<HostBlock>,
    host_header: &str,
    script_filename: &std::path::Path,
    script_name: &str,
    path_info: &str,
    query: &str,
    peer: SocketAddr,
    req: Request<Body>,
) -> Response {
    let pool = match state.pools.get_or_spawn(&host.project, "normal").await {
        Ok(p) => p,
        Err(e) => return bad_gateway(&format!("php-fpm failed to start: {e:#}")),
    };

    let body_bytes = match collect_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let (method, headers) = body_bytes.1;
    let body = body_bytes.0;

    let web_root_canonical = host
        .project
        .join(&host.root)
        .canonicalize()
        .unwrap_or_else(|_| host.project.join(&host.root));

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let content_length = body.len().to_string();
    let method_str = method.as_str();
    let server_port = state.listen_port.to_string();
    let peer_addr = peer.ip().to_string();
    let peer_port = peer.port().to_string();
    let script_fn_str = script_filename.display().to_string();
    let doc_root_str = web_root_canonical.display().to_string();
    let request_uri_str = if query.is_empty() {
        // PATH_INFO carries the URI portion after the script for
        // front-controller dispatch; for direct hits it's empty and we
        // fall back to script_name as REQUEST_URI.
        if path_info.is_empty() {
            script_name.to_owned()
        } else {
            path_info.to_owned()
        }
    } else if path_info.is_empty() {
        format!("{script_name}?{query}")
    } else {
        format!("{path_info}?{query}")
    };

    // Build the FastCGI param table. The exact set matches what nginx
    // sends with `include fastcgi_params; fastcgi_param SCRIPT_FILENAME …`.
    let mut params: Vec<(&str, &str)> = Vec::with_capacity(20 + headers.len() * 2);
    params.push(("SCRIPT_FILENAME", &script_fn_str));
    params.push(("SCRIPT_NAME", script_name));
    params.push(("PATH_INFO", path_info));
    params.push(("QUERY_STRING", query));
    params.push(("REQUEST_METHOD", method_str));
    params.push(("REQUEST_URI", &request_uri_str));
    params.push(("DOCUMENT_ROOT", &doc_root_str));
    params.push(("CONTENT_TYPE", content_type));
    params.push(("CONTENT_LENGTH", &content_length));
    params.push(("REMOTE_ADDR", &peer_addr));
    params.push(("REMOTE_PORT", &peer_port));
    params.push(("SERVER_NAME", host_header));
    params.push(("SERVER_PORT", &server_port));
    params.push(("SERVER_PROTOCOL", "HTTP/1.1"));
    params.push(("SERVER_SOFTWARE", "bougie/0.1"));
    params.push(("GATEWAY_INTERFACE", "CGI/1.1"));

    // Forward every request header as `HTTP_*`. The Host header is
    // already covered by `SERVER_NAME` above but `HTTP_HOST` is also
    // expected by PHP frameworks.
    let http_headers = http_header_params(&headers);
    for (k, v) in &http_headers {
        params.push((k.as_str(), v.as_str()));
    }

    let result = match fastcgi::dispatch(pool.socket(), &params, &body).await {
        Ok(r) => r,
        Err(e) => return bad_gateway(&format!("fastcgi dispatch failed: {e:#}")),
    };

    // FPM writes the script's stderr output here. Forward it to the
    // server's stderr so users see warnings inline with the
    // [fpm:<host>:<variant>] prefix.
    if !result.stderr.is_empty() {
        let prefix = format!("[php:{}:{}]", host_header, "normal");
        for line in result.stderr.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            eprintln!("{prefix} {}", String::from_utf8_lossy(line));
        }
    }

    cgi_to_response(&result.stdout, pool.php_version(), &host.project, "normal")
}

/// Wrap up [`collect_body`]'s tuple return. The outer Result lets the
/// caller surface a body-too-large rejection without unwrapping.
async fn collect_body(
    req: Request<Body>,
) -> Result<(Vec<u8>, (axum::http::Method, HeaderMap)), Response> {
    let method = req.method().clone();
    let headers = req.headers().clone();
    let body = req.into_body();
    let collected = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Err(internal_error()),
    };
    if collected.len() > MAX_REQUEST_BODY {
        return Err(plain_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "bougie: request body exceeds the {MAX_REQUEST_BODY}-byte limit\n"
            ),
        ));
    }
    Ok((collected.to_vec(), (method, headers)))
}

/// Convert each request header into the matching `HTTP_<NAME>` FCGI
/// param. Header values that aren't valid UTF-8 are skipped.
fn http_header_params(h: &HeaderMap) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(h.len());
    for (name, value) in h {
        let n = name.as_str();
        // Content-Type / Content-Length are passed under their own
        // CGI names above; don't double-emit.
        if n.eq_ignore_ascii_case("content-type") || n.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let Ok(v) = value.to_str() else { continue };
        let mut key = String::with_capacity(5 + n.len());
        key.push_str("HTTP_");
        for c in n.chars() {
            if c == '-' {
                key.push('_');
            } else {
                key.push(c.to_ascii_uppercase());
            }
        }
        out.push((key, v.to_owned()));
    }
    out
}

/// Parse the CGI-style header section that FPM writes ahead of the
/// response body and merge it into an axum `Response`. Forwarded
/// headers come straight through; `Status: NNN reason` controls the
/// HTTP status code; the body after the blank-line separator is
/// streamed back verbatim.
fn cgi_to_response(stdout: &[u8], php_version: &str, project: &std::path::Path, pool: &str) -> Response {
    let (head, body) = split_cgi(stdout);
    let mut status = StatusCode::OK;
    let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
    let mut explicit_content_length = false;

    for raw in head.split(|b| *b == b'\n') {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if raw.is_empty() {
            continue;
        }
        let Some(colon) = raw.iter().position(|b| *b == b':') else {
            continue;
        };
        let name = &raw[..colon];
        let value = raw[colon + 1..].trim_ascii_start();
        let name_str = std::str::from_utf8(name).unwrap_or("").to_ascii_lowercase();
        let value_bytes = value.to_vec();
        if name_str == "status" {
            if let Some(code) = parse_status_code(&value_bytes) {
                status = code;
            }
            continue;
        }
        let Ok(hn) = HeaderName::from_lowercase(name_str.as_bytes()) else {
            continue;
        };
        let Ok(hv) = HeaderValue::from_bytes(&value_bytes) else {
            continue;
        };
        if hn == header::CONTENT_LENGTH {
            explicit_content_length = true;
        }
        headers.push((hn, hv));
    }

    let body_len = body.len();
    let body_vec = body.to_vec();

    let mut builder = Response::builder().status(status);
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    if !explicit_content_length {
        builder = builder.header(header::CONTENT_LENGTH, body_len);
    }
    builder = builder.header("x-bougie-pool", pool);
    // Phase-2 debug headers — useful in dev to confirm which pool
    // handled a request without trawling logs.
    if let Ok(v) = HeaderValue::from_str(php_version) {
        builder = builder.header("x-bougie-php-version", v);
    }
    if let Ok(v) = HeaderValue::from_str(&project.display().to_string()) {
        builder = builder.header("x-bougie-project", v);
    }
    builder
        .body(Body::from(body_vec))
        .unwrap_or_else(|_| internal_error())
}

/// Split a CGI-formatted byte slice on the first `\r\n\r\n` (or `\n\n`)
/// into a `(headers, body)` pair. If no separator is found, the entire
/// input is treated as the header section and the body is empty —
/// matching what nginx does when the script forgets the blank line.
fn split_cgi(stdout: &[u8]) -> (&[u8], &[u8]) {
    if let Some(idx) = find_subslice(stdout, b"\r\n\r\n") {
        (&stdout[..idx], &stdout[idx + 4..])
    } else if let Some(idx) = find_subslice(stdout, b"\n\n") {
        (&stdout[..idx], &stdout[idx + 2..])
    } else {
        (stdout, &[])
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn parse_status_code(value: &[u8]) -> Option<StatusCode> {
    let s = std::str::from_utf8(value).ok()?;
    let code_str = s.split_whitespace().next()?;
    let code: u16 = code_str.parse().ok()?;
    StatusCode::from_u16(code).ok()
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

fn bad_gateway(message: &str) -> Response {
    plain_response(StatusCode::BAD_GATEWAY, format!("bougie: {message}\n"))
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
    use crate::commands::server::paths::ServerPaths;
    use std::path::PathBuf;

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

    fn empty_state(cfg: &ServerConfig) -> eyre::Result<AppState> {
        let bp = crate::paths::Paths::new(PathBuf::from("/tmp/bh"), PathBuf::from("/tmp/bc"));
        let sp = ServerPaths::from_root(PathBuf::from("/tmp/sp"));
        let pm = Arc::new(PoolManager::new(bp, sp, vec!["xdebug".into()]));
        AppState::build(cfg, pm, 7080)
    }

    #[test]
    fn app_state_indexes_canonical_and_alias() {
        let cfg = ServerConfig {
            server: super::super::config::ServerSection::default(),
            hosts: vec![block("a.bougie.run", &["b.bougie.run"])],
        };
        let state = empty_state(&cfg).unwrap();
        assert!(state.hosts.contains_key("a.bougie.run"));
        assert!(state.hosts.contains_key("b.bougie.run"));
    }

    #[test]
    fn duplicate_hostname_errors_at_build() {
        let cfg = ServerConfig {
            server: super::super::config::ServerSection::default(),
            hosts: vec![block("dup.bougie.run", &[]), block("dup.bougie.run", &[])],
        };
        assert!(empty_state(&cfg).is_err());
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
        assert!(empty_state(&cfg).is_err());
    }

    #[test]
    fn host_lookup_is_case_insensitive() {
        let cfg = ServerConfig {
            server: super::super::config::ServerSection::default(),
            hosts: vec![block("Case.Bougie.Run", &[])],
        };
        let state = empty_state(&cfg).unwrap();
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

    #[test]
    fn split_cgi_finds_blank_line() {
        let body = b"Content-Type: text/html\r\nX-Powered-By: PHP\r\n\r\n<html>";
        let (head, body_part) = split_cgi(body);
        assert!(std::str::from_utf8(head).unwrap().contains("Content-Type"));
        assert_eq!(body_part, b"<html>");
    }

    #[test]
    fn split_cgi_accepts_lf_only_separator() {
        let body = b"Content-Type: text/html\n\n<html>";
        let (_, body_part) = split_cgi(body);
        assert_eq!(body_part, b"<html>");
    }

    #[test]
    fn cgi_status_header_overrides_default_200() {
        let raw = b"Status: 404 Not Found\r\nContent-Type: text/plain\r\n\r\nmissing";
        let resp = cgi_to_response(raw, "8.3.12-nts", std::path::Path::new("/p"), "normal");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn cgi_default_status_is_200() {
        let raw = b"Content-Type: text/plain\r\n\r\nok";
        let resp = cgi_to_response(raw, "8.3.12-nts", std::path::Path::new("/p"), "normal");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn cgi_response_attaches_pool_header() {
        let raw = b"Content-Type: text/plain\r\n\r\nok";
        let resp = cgi_to_response(raw, "8.3.12-nts", std::path::Path::new("/p"), "normal");
        assert_eq!(
            resp.headers().get("x-bougie-pool").map(axum::http::HeaderValue::as_bytes),
            Some(&b"normal"[..])
        );
    }

    #[test]
    fn http_header_params_skips_content_headers() {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("4"));
        h.insert(header::USER_AGENT, HeaderValue::from_static("curl/8"));
        let out = http_header_params(&h);
        let keys: Vec<&str> = out.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"HTTP_USER_AGENT"));
        assert!(!keys.contains(&"HTTP_CONTENT_TYPE"));
        assert!(!keys.contains(&"HTTP_CONTENT_LENGTH"));
    }

    #[test]
    fn http_header_param_dashes_become_underscores() {
        let mut h = HeaderMap::new();
        h.insert(
            HeaderName::from_static("x-bougie-debug"),
            HeaderValue::from_static("1"),
        );
        let out = http_header_params(&h);
        assert_eq!(out[0].0, "HTTP_X_BOUGIE_DEBUG");
        assert_eq!(out[0].1, "1");
    }
}
