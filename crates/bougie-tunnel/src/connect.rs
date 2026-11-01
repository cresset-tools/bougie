//! TLS dial + handshake + yamux client setup.

use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::crypto::ring;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::protocol::{self, HelloRequest, HelloResponse, PROTOCOL_VERSION};
use crate::{Mux, Refused, TlsIo, TunnelConfig};

/// Result of a completed handshake: the assigned share plus the live yamux
/// session (internal — [`crate::TunnelClient::open`] splits it into the public
/// [`crate::ShareHandle`] + [`crate::Serving`]).
pub(crate) struct Connected {
    pub hello: HelloResponse,
    pub conn: Mux,
}

/// A rustls client config that verifies the relay against the Mozilla root set
/// (the relay presents a real Let's Encrypt `*.bougie.show` cert). Pins the ring
/// provider explicitly so feature-unification can't leave the provider ambiguous.
fn client_config() -> Result<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    // Dev / self-host hook: additionally trust the CA(s) in the PEM at
    // `BOUGIE_TUNNEL_CA`. This *adds* a root (still fully verifies the chain +
    // hostname) — it does not disable verification — so a self-signed relay can
    // be trusted for local testing without a foot-gun "insecure" switch.
    if let Ok(path) = std::env::var("BOUGIE_TUNNEL_CA") {
        let mut r = std::io::BufReader::new(
            std::fs::File::open(&path).map_err(|e| eyre!("open BOUGIE_TUNNEL_CA {path}: {e}"))?,
        );
        for cert in rustls_pemfile::certs(&mut r) {
            roots.add(cert?)?;
        }
    }
    let cfg = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(cfg)
}

/// Dial the relay, send [`HelloRequest`], read [`HelloResponse`], then hand the
/// same TLS stream to yamux as the client.
pub(crate) async fn connect(cfg: &TunnelConfig) -> Result<Connected> {
    let tcp = TcpStream::connect(&cfg.relay_addr).await?;
    tcp.set_nodelay(true).ok();
    set_keepalive(&tcp);

    let connector = TlsConnector::from(Arc::new(client_config()?));
    let sni = ServerName::try_from(cfg.relay_sni.clone())?;
    let mut tls = connector.connect(sni, tcp).await?;

    let req = HelloRequest {
        v: PROTOCOL_VERSION,
        token: cfg.token.clone(),
        slug: cfg.slug.clone(),
        project: cfg.project.clone(),
        password_mode: cfg.password_mode,
        password: cfg.password.clone(),
        resume: cfg.resume.clone(),
    };
    protocol::write_frame(&mut tls, &req).await?;
    let hello: HelloResponse = protocol::read_frame(&mut tls).await?;
    if !hello.ok {
        let why = hello.error.as_deref().unwrap_or("unknown error");
        return Err(Refused(why.to_owned()).into());
    }

    // Hand the *same* TLS stream to yamux; the laptop is the yamux client and
    // only ever accepts substreams the relay opens per inbound request.
    let io: TlsIo = tls.compat();
    let conn = yamux::Connection::new(io, yamux::Config::default(), yamux::Mode::Client);
    Ok(Connected { hello, conn })
}

/// Probe the idle connection so a dead link (laptop slept, network changed) is
/// noticed in ~30s and the share reconnects, instead of hanging until the
/// kernel's two-hour default. Best-effort.
fn set_keepalive(tcp: &TcpStream) {
    let ka = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(15))
        .with_interval(Duration::from_secs(5));
    #[cfg(not(windows))]
    let ka = ka.with_retries(3);
    if let Err(e) = socket2::SockRef::from(tcp).set_tcp_keepalive(&ka) {
        tracing::debug!("set keepalive: {e}");
    }
}
