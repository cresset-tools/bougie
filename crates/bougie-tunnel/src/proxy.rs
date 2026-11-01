//! Accept relay-opened substreams and splice each to the local server.
//!
//! This is the whole client-side proxy: the relay opens one yamux substream per
//! inbound browser request and speaks HTTP over it; we pipe each substream to a
//! fresh loopback connection to the local `bougie server`. Bytes pass through
//! unchanged. The only HTTP we look at is the first line in each direction —
//! the request line and the status line — to report the exchange as a
//! [`RequestLog`].

use std::future::poll_fn;
use std::time::{Duration, Instant};

use eyre::Result;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::Mux;

/// One proxied request, reported once the exchange finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestLog {
    pub method: String,
    /// The request target as sent: path plus any query string.
    pub target: String,
    /// `None` when the local server sent no parseable status line.
    pub status: Option<u16>,
    /// The status line's reason phrase (`OK`, `Not Found`), possibly empty.
    pub reason: String,
    /// From the relay opening the substream to the response's last byte.
    pub duration: Duration,
    /// Response bytes sent back to the relay, headers included.
    pub bytes: u64,
}

/// Drive the yamux session: for each inbound substream, splice it to the local
/// server. Returns `Ok(())` when the relay closes the session.
pub(crate) async fn serve(
    mut conn: Mux,
    local_port: u16,
    log: Option<UnboundedSender<RequestLog>>,
) -> Result<()> {
    loop {
        match poll_fn(|cx| conn.poll_next_inbound(cx)).await {
            Some(Ok(stream)) => {
                let log = log.clone();
                tokio::spawn(async move {
                    if let Err(e) = splice(stream, local_port, log).await {
                        tracing::debug!("share substream ended: {e}");
                    }
                });
            }
            Some(Err(e)) => return Err(e.into()),
            None => return Ok(()), // relay closed the session
        }
    }
}

/// One substream ⇄ one loopback connection to `127.0.0.1:<local_port>`.
async fn splice(
    stream: yamux::Stream,
    local_port: u16,
    log: Option<UnboundedSender<RequestLog>>,
) -> Result<()> {
    let started = Instant::now();
    let (mut down_r, mut down_w) = tokio::io::split(stream.compat());
    let upstream = TcpStream::connect(("127.0.0.1", local_port)).await?;
    let (mut up_r, mut up_w) = upstream.into_split();

    let request = async {
        let r = pump(&mut down_r, &mut up_w).await;
        let _ = up_w.shutdown().await;
        r
    };
    let response = async {
        let r = pump(&mut up_r, &mut down_w).await;
        let _ = down_w.shutdown().await;
        r
    };
    let (request, response) = tokio::join!(request, response);
    let (request_line, _) = request?;
    let (status_line, bytes) = response?;

    if let (Some(tx), Some((method, target))) = (log, request_line.as_deref().and_then(parse_request_line)) {
        let (status, reason) = status_line
            .as_deref()
            .and_then(parse_status_line)
            .map_or((None, String::new()), |(s, r)| (Some(s), r));
        let _ = tx.send(RequestLog {
            method,
            target,
            status,
            reason,
            duration: started.elapsed(),
            bytes,
        });
    }
    Ok(())
}

/// The longest first line we keep for reporting; the bytes still all pass.
const MAX_FIRST_LINE: usize = 8 * 1024;

/// Copy `from` → `to` until EOF, unchanged. Returns the first line seen (for
/// the log) and the byte count.
async fn pump<R, W>(from: &mut R, to: &mut W) -> std::io::Result<(Option<String>, u64)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 16 * 1024];
    let mut head: Vec<u8> = Vec::new();
    let mut first_line: Option<String> = None;
    let mut total = 0u64;
    loop {
        let n = from.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n]).await?;
        total += n as u64;
        if first_line.is_none() && head.len() < MAX_FIRST_LINE {
            head.extend_from_slice(&buf[..n.min(MAX_FIRST_LINE - head.len())]);
            if let Some(end) = head.windows(2).position(|w| w == b"\r\n") {
                first_line = Some(String::from_utf8_lossy(&head[..end]).into_owned());
            }
        }
    }
    to.flush().await?;
    Ok((first_line, total))
}

/// `GET /path?q=1 HTTP/1.1` → `("GET", "/path?q=1")`.
fn parse_request_line(line: &str) -> Option<(String, String)> {
    let mut parts = line.split(' ');
    let method = parts.next().filter(|m| !m.is_empty())?;
    let target = parts.next().filter(|t| !t.is_empty())?;
    parts.next().filter(|v| v.starts_with("HTTP/"))?;
    Some((method.to_owned(), target.to_owned()))
}

/// `HTTP/1.1 404 Not Found` → `(404, "Not Found")`.
fn parse_status_line(line: &str) -> Option<(u16, String)> {
    let rest = line.strip_prefix("HTTP/")?;
    let (_version, rest) = rest.split_once(' ')?;
    let (code, reason) = rest.split_once(' ').unwrap_or((rest, ""));
    Some((code.parse().ok()?, reason.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_request_line() {
        assert_eq!(
            parse_request_line("GET /wp-admin/?a=1 HTTP/1.1"),
            Some(("GET".into(), "/wp-admin/?a=1".into()))
        );
        assert_eq!(parse_request_line("garbage"), None);
    }

    #[test]
    fn parses_the_status_line() {
        assert_eq!(parse_status_line("HTTP/1.1 200 OK"), Some((200, "OK".into())));
        assert_eq!(parse_status_line("HTTP/1.1 404 Not Found"), Some((404, "Not Found".into())));
        assert_eq!(parse_status_line("HTTP/1.1 204"), Some((204, String::new())));
        assert_eq!(parse_status_line("nope"), None);
    }

    #[tokio::test]
    async fn pump_passes_bytes_through_and_finds_the_first_line() {
        let input = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi".to_vec();
        let mut from = &input[..];
        let mut out = Vec::new();
        let (line, n) = pump(&mut from, &mut out).await.unwrap();
        assert_eq!(out, input, "bytes are unchanged");
        assert_eq!(n, input.len() as u64);
        assert_eq!(line.as_deref(), Some("HTTP/1.1 200 OK"));
    }
}
