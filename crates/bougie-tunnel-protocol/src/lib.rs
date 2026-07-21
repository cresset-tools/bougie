//! Wire protocol shared by the bougie tunnel client ([`bougie-tunnel`]) and the
//! relay (`cresset-tools/bougie-relay`, closed).
//!
//! This crate is the **single source of truth** for the client↔relay contract —
//! it exists so the two ends can't drift. The client is the contract owner (it's
//! the open half); the closed relay depends on this crate rather than keeping its
//! own copy.
//!
//! One length-prefixed (`u32` big-endian length, then that many bytes of JSON)
//! handshake frame travels in each direction, after which the *same* connection
//! is handed to yamux for the data streams. See `.claude/SHARE_SKELETON.md` §1 in
//! the bougie repo for the full spec.
//!
//! [`bougie-tunnel`]: https://docs.rs/bougie-tunnel

use eyre::{Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Protocol version. Bump on any breaking change to the frame shapes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Upper bound on a handshake frame; the real frames are a few hundred bytes, so
/// anything larger is a bug or an abuse attempt.
const MAX_FRAME: usize = 1 << 20; // 1 MiB

/// View-password policy the client requests for its share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PasswordMode {
    /// Relay generates a random view password (the default, safe posture).
    #[default]
    Auto,
    /// No view password — the share is fully public (`--public`).
    None,
    /// Use the password supplied in [`HelloRequest::password`].
    Custom,
}

/// Client → relay: request a share.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloRequest {
    /// Protocol version the client speaks ([`PROTOCOL_VERSION`]).
    pub v: u32,
    /// Bearer token authorising the share (the relay introspects it against
    /// sconce); `None` for an anonymous share.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// A specific slug the client requests (may require auth); `None` = relay
    /// assigns a random one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// The project name, used to derive the default slug.
    pub project: String,
    /// The view-password policy for this share.
    #[serde(default)]
    pub password_mode: PasswordMode,
    /// The custom view password, when [`HelloRequest::password_mode`] is
    /// [`PasswordMode::Custom`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// A resume token from a prior session, to reclaim the same slug across a
    /// reconnect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
}

/// Relay → client: the assigned share, or an error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloResponse {
    /// Protocol version the relay speaks ([`PROTOCOL_VERSION`]).
    pub v: u32,
    /// Whether the share was granted.
    pub ok: bool,
    /// The assigned hostname, e.g. `myshop-ab12cd.bougie.show`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The full share URL, e.g. `https://myshop-ab12cd.bougie.show`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The generated view password, when the relay created one
    /// ([`PasswordMode::Auto`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_password: Option<String>,
    /// A resume token the client can present on reconnect to reclaim this slug.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// How long, in seconds, the relay will hold this share.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
    /// A human-readable reason, when [`HelloResponse::ok`] is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl HelloResponse {
    /// A successful response granting `host`/`url` with an optional view
    /// password and a TTL.
    #[must_use]
    pub fn ok(host: String, url: String, view_password: Option<String>, ttl_secs: u64) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: true,
            host: Some(host),
            url: Some(url),
            view_password,
            resume: None,
            ttl_secs: Some(ttl_secs),
            error: None,
        }
    }

    /// A rejection carrying a human-readable `msg`.
    #[must_use]
    pub fn error(msg: &str) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: false,
            host: None,
            url: None,
            view_password: None,
            resume: None,
            ttl_secs: None,
            error: Some(msg.to_owned()),
        }
    }
}

/// Write one length-prefixed JSON frame.
pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(msg)?;
    let len = u32::try_from(bytes.len())?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Read one length-prefixed JSON frame.
pub async fn read_frame<R, T>(r: &mut R) -> Result<T>
where
    R: AsyncReadExt + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        bail!("handshake frame too large: {len} bytes");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hello_round_trips_over_a_pipe() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let req = HelloRequest {
            v: PROTOCOL_VERSION,
            token: None,
            slug: Some("myshop".into()),
            project: "myshop".into(),
            password_mode: PasswordMode::Auto,
            password: None,
            resume: None,
        };
        let sent = req.clone();
        let writer = tokio::spawn(async move { write_frame(&mut a, &sent).await.unwrap() });
        let got: HelloRequest = read_frame(&mut b).await.unwrap();
        writer.await.unwrap();
        assert_eq!(got.v, PROTOCOL_VERSION);
        assert_eq!(got.project, "myshop");
        assert_eq!(got.slug.as_deref(), Some("myshop"));
        assert_eq!(got.password_mode, PasswordMode::Auto);
    }

    #[test]
    fn password_mode_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&PasswordMode::Auto).unwrap(), "\"auto\"");
        assert_eq!(serde_json::to_string(&PasswordMode::None).unwrap(), "\"none\"");
        assert_eq!(serde_json::to_string(&PasswordMode::Custom).unwrap(), "\"custom\"");
    }

    #[test]
    fn response_constructors() {
        let ok = HelloResponse::ok("h.bougie.show".into(), "https://h.bougie.show".into(), None, 3600);
        assert!(ok.ok);
        assert_eq!(ok.ttl_secs, Some(3600));
        let err = HelloResponse::error("nope");
        assert!(!err.ok);
        assert_eq!(err.error.as_deref(), Some("nope"));
    }
}
