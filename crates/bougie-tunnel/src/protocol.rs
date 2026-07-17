//! Wire protocol between the tunnel client (laptop) and the relay.
//!
//! One length-prefixed (`u32` big-endian) JSON handshake frame in each
//! direction, then the same connection is handed to yamux for the data
//! streams. See `.claude/SHARE_SKELETON.md` §1 for the full spec.

use eyre::{Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Protocol version. Bump on any breaking change to the frame shapes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Upper bound on a handshake frame; the real frames are a few hundred bytes,
/// so anything larger is a bug or an abuse attempt.
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
    pub v: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    pub project: String,
    #[serde(default)]
    pub password_mode: PasswordMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
}

/// Relay → client: the assigned share, or an error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloResponse {
    pub v: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
}
