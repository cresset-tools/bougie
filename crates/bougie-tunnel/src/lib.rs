//! Outbound multiplexed tunnel client: exposes a locally-running bougie store
//! on a public relay (`*.bougie.show`), ngrok-style.
//!
//! **Design invariant: the relay speaks HTTP; this client does not.** For every
//! inbound browser request the relay opens a yamux substream over the single
//! outbound TLS connection, and this client splices that substream straight to
//! the local dev server on loopback (`copy_bidirectional`). All HTTP semantics —
//! Host routing, `X-Forwarded-*` injection, `Set-Cookie` rewriting, view-auth —
//! live in the relay. The client forwards bytes unchanged; it only reads the
//! request and status lines to report each exchange ([`RequestLog`]).
//!
//! Flow: [`TunnelClient::open`] dials the relay, completes the length-prefixed
//! JSON handshake ([`mod@protocol`]), and returns the assigned [`ShareHandle`]
//! (URL + view password) plus a [`Serving`] future you drive until Ctrl-C.
//! [`supervise::keep_up`] wraps that with reconnect-and-backoff.

mod connect;
mod protocol;
mod proxy;
pub mod supervise;

use std::fmt;

use eyre::Result;

pub use protocol::{HelloRequest, HelloResponse, PasswordMode};
pub use proxy::RequestLog;

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
    /// Where to report each proxied request, if anywhere.
    pub request_log: Option<tokio::sync::mpsc::UnboundedSender<RequestLog>>,
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
    request_log: Option<tokio::sync::mpsc::UnboundedSender<RequestLog>>,
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
        proxy::serve(self.conn, self.local_port, self.request_log).await
    }
}

/// The relay answered the handshake with a refusal (bad login, not allowed to
/// share). Distinct from network errors because retrying can't fix it.
#[derive(Debug, Clone)]
pub struct Refused(pub String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "relay refused the share: {}", self.0)
    }
}

impl std::error::Error for Refused {}

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
        Ok((
            handle,
            Serving {
                conn: c.conn,
                local_port: self.cfg.local_port,
                request_log: self.cfg.request_log.clone(),
            },
        ))
    }

    /// Config accessor.
    #[must_use]
    pub fn config(&self) -> &TunnelConfig {
        &self.cfg
    }

    /// A client that reconnects to `handle`'s share: it presents the resume
    /// token, and also asks for the same slug and view password, so a relay
    /// that can't resume (or predates resume tokens) still hands back the same
    /// URL when the slug is free.
    #[must_use]
    pub fn resuming(&self, handle: &ShareHandle) -> Self {
        let mut cfg = self.cfg.clone();
        if handle.resume.is_some() {
            cfg.resume.clone_from(&handle.resume);
        }
        if let Some(label) = handle.host.split('.').next().filter(|l| !l.is_empty()) {
            cfg.slug = Some(label.to_owned());
        }
        cfg.password_mode = if handle.view_password.is_some() {
            PasswordMode::Custom
        } else {
            PasswordMode::None
        };
        cfg.password.clone_from(&handle.view_password);
        Self { cfg }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TunnelConfig {
        TunnelConfig {
            relay_addr: "relay:443".into(),
            relay_sni: "tunnel.bougie.show".into(),
            local_port: 7080,
            project: "shop".into(),
            token: Some("t".into()),
            slug: None,
            password_mode: PasswordMode::Auto,
            password: None,
            resume: None,
            request_log: None,
        }
    }

    #[test]
    fn resuming_asks_for_the_same_share_back() {
        let handle = ShareHandle {
            url: "https://shop-x1y2z3.bougie.show".into(),
            host: "shop-x1y2z3.bougie.show".into(),
            view_password: Some("pw".into()),
            resume: Some("tok".into()),
        };
        let c = TunnelClient::new(cfg()).resuming(&handle);
        let c = c.config();
        assert_eq!(c.resume.as_deref(), Some("tok"));
        // Fallbacks for a relay that can't resume: same slug, same password.
        assert_eq!(c.slug.as_deref(), Some("shop-x1y2z3"));
        assert_eq!(c.password_mode, PasswordMode::Custom);
        assert_eq!(c.password.as_deref(), Some("pw"));
    }

    #[test]
    fn resuming_a_public_share_stays_public() {
        let handle = ShareHandle {
            url: "https://shop-a.bougie.show".into(),
            host: "shop-a.bougie.show".into(),
            view_password: None,
            resume: None,
        };
        let c = TunnelClient::new(cfg()).resuming(&handle);
        assert_eq!(c.config().password_mode, PasswordMode::None);
        assert_eq!(c.config().resume, None);
    }
}
