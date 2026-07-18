//! Outbound multiplexed tunnel client: exposes a locally-running bougie store
//! on a public relay (`*.bougie.show`), ngrok-style.
//!
//! **Design invariant: the relay speaks HTTP; this client does not.** For every
//! inbound browser request the relay opens a yamux substream over the single
//! outbound TLS connection, and this client splices that substream straight to
//! the local dev server on loopback (`copy_bidirectional`). All HTTP semantics —
//! Host routing, `X-Forwarded-*` injection, `Set-Cookie` rewriting, view-auth —
//! live in the relay, so the client never parses a request.
//!
//! Flow: [`TunnelClient::open`] dials the relay, completes the length-prefixed
//! JSON handshake ([`mod@protocol`]), and returns the assigned [`ShareHandle`]
//! (URL + view password) plus a [`Serving`] future you drive until Ctrl-C.
//! [`supervise::run_forever`] wraps that with reconnect-and-backoff.

mod connect;
mod protocol;
mod proxy;
pub mod supervise;

use std::fmt;

use eyre::Result;

pub use protocol::{HelloRequest, HelloResponse, PasswordMode};

/// The TLS byte stream after the handshake, adapted to futures-io for yamux.
type TlsIo = tokio_util::compat::Compat<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;
/// The yamux session multiplexed over the single TLS connection.
type Mux = yamux::Connection<TlsIo>;

/// Everything needed to dial the relay and request a share.
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    /// `host:port` of the relay's tunnel ingress.
    pub relay_addr: String,
    /// SNI / certificate name to verify the relay against.
    pub relay_sni: String,
    /// Loopback port of the local `bougie server` to forward to.
    pub local_port: u16,
    /// Project name (seeds the default `<project>-<random>` slug).
    pub project: String,
    /// Optional auth token (anonymous shares send `None`).
    pub token: Option<String>,
    /// Optional requested slug (stable named share; may require auth).
    pub slug: Option<String>,
    /// View-password policy for the share.
    pub password_mode: PasswordMode,
    /// Custom view password when `password_mode == PasswordMode::Custom`.
    pub password: Option<String>,
    /// Resume token to keep the same URL across reconnects.
    pub resume: Option<String>,
}

/// The assigned public share, returned as soon as the handshake completes.
#[derive(Debug, Clone)]
pub struct ShareHandle {
    /// Full `https://<slug>.bougie.show` URL to hand out.
    pub url: String,
    /// Just the hostname — registered as a `[[host]]` alias on the local server.
    pub host: String,
    /// Generated/basic-auth view password, unless the share is `--public`.
    pub view_password: Option<String>,
    /// Resume token to replay on reconnect to keep this URL.
    pub resume: Option<String>,
}

/// A live tunnel. Drive [`Serving::run`] until it returns (relay closed) or drop
/// it (Ctrl-C in the command) to end the share.
pub struct Serving {
    conn: Mux,
    local_port: u16,
}

impl fmt::Debug for Serving {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Serving")
            .field("local_port", &self.local_port)
            .finish_non_exhaustive()
    }
}

impl Serving {
    /// Accept relay-opened substreams and splice each to the local server.
    /// Returns `Ok(())` when the relay closes the session.
    pub async fn run(self) -> Result<()> {
        proxy::serve(self.conn, self.local_port).await
    }
}

/// Tunnel client. Cheap — holds only config.
#[derive(Debug, Clone)]
pub struct TunnelClient {
    cfg: TunnelConfig,
}

impl TunnelClient {
    #[must_use]
    pub fn new(cfg: TunnelConfig) -> Self {
        Self { cfg }
    }

    /// Dial the relay, complete the handshake, and return the assigned
    /// [`ShareHandle`] plus a [`Serving`] to drive.
    pub async fn open(&self) -> Result<(ShareHandle, Serving)> {
        let c = connect::connect(&self.cfg).await?;
        let handle = ShareHandle {
            url: c.hello.url.clone().unwrap_or_default(),
            host: c.hello.host.clone().unwrap_or_default(),
            view_password: c.hello.view_password.clone(),
            resume: c.hello.resume.clone(),
        };
        Ok((handle, Serving { conn: c.conn, local_port: self.cfg.local_port }))
    }

    /// Config accessor (used by the reconnect supervisor).
    #[must_use]
    pub fn config(&self) -> &TunnelConfig {
        &self.cfg
    }
}
