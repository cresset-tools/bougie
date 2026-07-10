//! Protocol-aware service health probes.
//!
//! The supervisor's original probe was a bare TCP/Unix-socket *connect*:
//! it proved a port was bound, not that the service behind it was ready.
//! That's a weak signal — opensearch answers on 9200 long before the
//! cluster is green, and mariadb's socket accepts connections while it's
//! still in crash recovery rejecting queries. These probes instead speak
//! each service's real protocol, so a service is only `Running` once it
//! can actually do work, and a wedged-but-alive service is caught by the
//! continuous re-probe (see `supervisor::check_all` and the daemon
//! ticker).
//!
//! Dispatch is by name — idiomatic in this crate (cf.
//! `supervisor::health_timeout_for`, `supervisor::sidecar_for`). Each
//! service delegates to its provisioner's protocol-aware check; anything
//! without one (runtime-only deps, future services) falls back to the
//! binding connect, the same signal the supervisor used before.

use super::catalog::{self, Binding, CatalogEntry};
use bougie_paths::Paths;
use eyre::{eyre, Result};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

/// Per-probe ceiling. A probe that can't answer within this counts as a
/// failure for the round. The continuous loop tolerates a few consecutive
/// misses before acting, so one slow probe won't flap a service. Sized
/// above the interval so a legitimately slow check (rabbitmqctl on a busy
/// node) isn't clipped; the in-flight guard prevents pile-up.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Run the health probe for `name`. `Ok(())` = healthy.
///
/// Bounded by [`PROBE_TIMEOUT`]; the exec-based probes (mariadb,
/// rabbitmq) set `kill_on_drop`, so a timeout that drops the future also
/// reaps the client process.
pub async fn probe(name: &str, version: &str, paths: &Paths) -> Result<()> {
    match tokio::time::timeout(PROBE_TIMEOUT, probe_inner(name, version, paths)).await {
        Ok(r) => r,
        Err(_) => Err(eyre!(
            "health probe for `{name}` timed out after {PROBE_TIMEOUT:?}"
        )),
    }
}

async fn probe_inner(name: &str, version: &str, paths: &Paths) -> Result<()> {
    let Some(entry) = catalog::find(name) else {
        return Err(eyre!("unknown service `{name}`"));
    };
    match name {
        "redis" => {
            let sock = socket_path(entry, version, paths)?;
            super::provisioners::redis::health(&sock).await
        }
        "mariadb" => {
            let sock = socket_path(entry, version, paths)?;
            super::provisioners::mariadb::health(paths, &sock).await
        }
        "mysql" => {
            let sock = socket_path(entry, version, paths)?;
            super::provisioners::mysql::health(paths, version, &sock).await
        }
        "opensearch" => {
            let port =
                super::endpoint::effective_primary(paths, "opensearch", version, 9200);
            super::provisioners::opensearch::health(port).await
        }
        "rabbitmq" => super::provisioners::rabbitmq::health(paths).await,
        // Mailpit's binding is the SMTP port, which has no cheap protocol
        // ping; probe the web UI instead (it comes up alongside SMTP) and
        // accept any 2xx. The web UI rides on the effective `http` port.
        "mailpit" => {
            let http = super::endpoint::effective_extra(
                paths,
                "mailpit",
                version,
                "http",
                catalog::MAILPIT_HTTP_PORT,
            );
            http_get(http, "/").await
        }
        // The dev server is deliberately left on the binding connect (see
        // the fallback below): its readiness is "the listener is bound +
        // control socket up", which happens *before* any project host is
        // registered. An HTTP probe would have to interpret the no-host
        // response (a virtual-host miss, not a health signal), and hitting
        // it continuously races the provisioner's control-socket host
        // reload. `server` therefore falls through to `connect`.
        // Runtime-only deps + anything without a richer probe: connect to
        // the binding (or trivially Ok for `Binding::None`).
        _ => connect(entry, version, paths).await,
    }
}

/// The binding-connect fallback — the supervisor's pre-health-check
/// probe. Kept as the default so a future service with no protocol probe
/// still gets the old behaviour, and `Binding::None` deps stay
/// unprobed.
async fn connect(entry: &CatalogEntry, version: &str, paths: &Paths) -> Result<()> {
    match entry.binding {
        Binding::UnixSocket { sockname } => {
            let path = paths.service_run(entry.name, version).join(sockname);
            tokio::net::UnixStream::connect(&path)
                .await
                .map(drop)
                .map_err(|e| {
                    eyre!(
                        "connecting to {} socket {}: {e}",
                        entry.name,
                        path.display()
                    )
                })
        }
        Binding::Tcp { port } => {
            let port = super::endpoint::effective_primary(paths, entry.name, version, port);
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .map(drop)
                .map_err(|e| eyre!("connecting to {} on 127.0.0.1:{port}: {e}", entry.name))
        }
        // Runtime-only deps (jdk, erlang) are never reachable as services.
        Binding::None => Ok(()),
    }
}

fn socket_path(entry: &CatalogEntry, version: &str, paths: &Paths) -> Result<PathBuf> {
    match entry.binding {
        Binding::UnixSocket { sockname } => Ok(paths.service_run(entry.name, version).join(sockname)),
        _ => Err(eyre!("{} is not a unix-socket service", entry.name)),
    }
}

/// Probe an HTTP endpoint, healthy on a 2xx response.
async fn http_get(port: u16, path: &str) -> Result<()> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = http_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| eyre!("GET {url}: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(eyre!("GET {url} returned {}", resp.status()))
    }
}

/// Shared async client for the generic HTTP probes (mailpit, server).
/// `build` only fails on TLS config, which can't apply to HTTP-only
/// localhost.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .expect("reqwest::Client::builder for HTTP-only localhost cannot fail")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths() -> Paths {
        let tmp = tempfile::TempDir::new().unwrap();
        let path: std::path::PathBuf = tmp.keep();
        Paths::new(path.clone(), path)
    }

    #[tokio::test]
    async fn unknown_service_errors() {
        let err = probe("postgres", "0", &test_paths()).await.unwrap_err();
        assert!(format!("{err:#}").contains("unknown service"));
    }

    #[tokio::test]
    async fn runtime_only_dep_is_trivially_healthy() {
        // jdk/erlang have `Binding::None` — the connect fallback treats
        // them as healthy (they're never reachable as services).
        assert!(probe("jdk", crate::daemon::catalog::default_version("jdk"), &test_paths()).await.is_ok());
        assert!(probe("erlang", crate::daemon::catalog::default_version("erlang"), &test_paths()).await.is_ok());
    }

    #[tokio::test]
    async fn unix_socket_connect_fallback_succeeds_against_a_live_socket() {
        // Drive the binding-connect fallback (used for any service without
        // a richer probe) against a real listener at redis's socket path.
        let paths = test_paths();
        let sock = paths.service_run("redis", crate::daemon::catalog::default_version("redis")).join("redis.sock");
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let entry = catalog::find("redis").unwrap();
        let ok = connect(entry, &entry.version, &paths).await;
        assert!(ok.is_ok(), "{ok:?}");
        drop(listener);
    }

    #[tokio::test]
    async fn unix_socket_connect_fallback_fails_when_nothing_listening() {
        let paths = test_paths();
        let entry = catalog::find("redis").unwrap();
        // No socket created → connect refused.
        assert!(connect(entry, &entry.version, &paths).await.is_err());
    }
}
