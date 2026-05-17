//! FastCGI 1.0 client over a unix socket. Spec: SERVER.md §7.6, §5.5,
//! plus the FastCGI 1.0 wire format (https://fastcgi-archives.github.io/FastCGI_Specification.html).
//!
//! What this module provides:
//!
//! - [`encode_params`] — pack the FastCGI name/value pair format used
//!   by the `PARAMS` record body.
//! - [`Record`] / [`RecordType`] — opcodes + a small writer for the
//!   header + body.
//! - [`dispatch`] — high-level "open a UnixStream, send PARAMS +
//!   optional STDIN, read STDOUT + STDERR until END_REQUEST". This is
//!   the function the router uses for each `.php` request.
//! - [`probe`] — `FCGI_GET_VALUES` health check used right after a
//!   pool spawn. 2s timeout per SERVER.md §7.6.
//!
//! We use `KEEP_CONN=0` (one TCP/unix connection per request) for v1
//! and do not implement multiplexing. The protocol allows multiplexed
//! requests on a single connection via the request-id field, but
//! php-fpm's default workers are single-request-at-a-time anyway, so
//! the only thing multiplexing buys us in practice is fewer connect()
//! syscalls. We can revisit if a perf trace shows it dominating.

use eyre::{eyre, Result, WrapErr};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const FCGI_VERSION_1: u8 = 1;
const FCGI_HEADER_LEN: usize = 8;
const FCGI_KEEP_CONN: u8 = 0;
const FCGI_RESPONDER: u16 = 1;
/// Phase 2 uses a single request-id per connection (one request per
/// connection, no multiplexing). The exact value doesn't matter so
/// long as it's non-zero.
const FCGI_REQUEST_ID: u16 = 1;

const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordType {
    BeginRequest = 1,
    AbortRequest = 2,
    EndRequest = 3,
    Params = 4,
    Stdin = 5,
    Stdout = 6,
    Stderr = 7,
    GetValues = 9,
    GetValuesResult = 10,
}

impl RecordType {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::BeginRequest,
            2 => Self::AbortRequest,
            3 => Self::EndRequest,
            4 => Self::Params,
            5 => Self::Stdin,
            6 => Self::Stdout,
            7 => Self::Stderr,
            9 => Self::GetValues,
            10 => Self::GetValuesResult,
            _ => return None,
        })
    }
}

/// Pack a list of `(name, value)` byte pairs into the FastCGI
/// name-value-pair format used by PARAMS / GET_VALUES records.
///
/// Each pair is `<name-len><value-len><name><value>` where each length
/// is encoded as a 1-byte form (top bit clear, value ≤ 127) or a
/// 4-byte form (top bit set, value ≤ 2^31 - 1).
pub fn encode_params(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in pairs {
        write_len(&mut out, name.len());
        write_len(&mut out, value.len());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(value.as_bytes());
    }
    out
}

fn write_len(buf: &mut Vec<u8>, len: usize) {
    if len < 128 {
        buf.push(u8::try_from(len).expect("len < 128"));
    } else {
        let l = u32::try_from(len).expect("fastcgi length fits in u32");
        let bytes = l.to_be_bytes();
        // Top bit set on the first byte signals 4-byte form.
        buf.push(bytes[0] | 0x80);
        buf.push(bytes[1]);
        buf.push(bytes[2]);
        buf.push(bytes[3]);
    }
}

/// Parse the FCGI name-value-pair format. Returns the decoded pairs.
pub fn decode_params(mut body: &[u8]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    while !body.is_empty() {
        let (name_len, rest) = read_len(body)?;
        body = rest;
        let (value_len, rest) = read_len(body)?;
        body = rest;
        if body.len() < name_len + value_len {
            return Err(eyre!("truncated fastcgi name-value-pair"));
        }
        let name = String::from_utf8_lossy(&body[..name_len]).into_owned();
        body = &body[name_len..];
        let value = String::from_utf8_lossy(&body[..value_len]).into_owned();
        body = &body[value_len..];
        out.push((name, value));
    }
    Ok(out)
}

fn read_len(body: &[u8]) -> Result<(usize, &[u8])> {
    if body.is_empty() {
        return Err(eyre!("truncated fastcgi length"));
    }
    if body[0] & 0x80 == 0 {
        return Ok((body[0] as usize, &body[1..]));
    }
    if body.len() < 4 {
        return Err(eyre!("truncated 4-byte fastcgi length"));
    }
    let len = (u32::from(body[0] & 0x7f) << 24)
        | (u32::from(body[1]) << 16)
        | (u32::from(body[2]) << 8)
        | u32::from(body[3]);
    Ok((len as usize, &body[4..]))
}

/// Wire-format header. Bodies fragment at FastCGI's 65,535-byte ceiling
/// per record — [`write_record`] handles the fragmentation.
fn write_header(buf: &mut Vec<u8>, kind: RecordType, content_len: u16, padding_len: u8) {
    buf.push(FCGI_VERSION_1);
    buf.push(kind as u8);
    buf.extend_from_slice(&FCGI_REQUEST_ID.to_be_bytes());
    buf.extend_from_slice(&content_len.to_be_bytes());
    buf.push(padding_len);
    buf.push(0); // reserved
}

/// Append one or more record frames for `body` under the given record
/// type. Bodies longer than 65,535 bytes are split across multiple
/// frames per the spec.
pub fn write_record(buf: &mut Vec<u8>, kind: RecordType, body: &[u8]) {
    if body.is_empty() {
        // Empty record = "end of stream" sentinel for PARAMS / STDIN.
        write_header(buf, kind, 0, 0);
        return;
    }
    for chunk in body.chunks(65_535) {
        let len = u16::try_from(chunk.len()).expect("chunked by 65535");
        write_header(buf, kind, len, 0);
        buf.extend_from_slice(chunk);
    }
}

fn write_begin_request(buf: &mut Vec<u8>) {
    write_header(buf, RecordType::BeginRequest, 8, 0);
    buf.extend_from_slice(&FCGI_RESPONDER.to_be_bytes());
    buf.push(FCGI_KEEP_CONN);
    buf.extend_from_slice(&[0u8; 5]); // reserved
}

/// A decoded record header + body pair, as read off the socket.
#[derive(Debug)]
pub struct Frame {
    pub kind: RecordType,
    pub body: Vec<u8>,
}

/// Read one full FastCGI record (header + body + padding).
async fn read_frame(stream: &mut UnixStream) -> Result<Frame> {
    let mut header = [0u8; FCGI_HEADER_LEN];
    stream
        .read_exact(&mut header)
        .await
        .wrap_err("reading fastcgi header")?;
    if header[0] != FCGI_VERSION_1 {
        return Err(eyre!("unexpected fastcgi version: {}", header[0]));
    }
    let kind = RecordType::from_u8(header[1])
        .ok_or_else(|| eyre!("unknown fastcgi record type: {}", header[1]))?;
    let content_len = usize::from(u16::from_be_bytes([header[4], header[5]]));
    let padding_len = usize::from(header[6]);
    let mut body = vec![0u8; content_len];
    if content_len > 0 {
        stream
            .read_exact(&mut body)
            .await
            .wrap_err("reading fastcgi body")?;
    }
    if padding_len > 0 {
        let mut pad = vec![0u8; padding_len];
        stream
            .read_exact(&mut pad)
            .await
            .wrap_err("reading fastcgi padding")?;
    }
    Ok(Frame { kind, body })
}

/// Open a connection to `socket`, send the BEGIN_REQUEST + PARAMS +
/// STDIN sequence, then read STDOUT + STDERR + END_REQUEST.
///
/// Returns `(stdout, stderr, app_status)`. `stdout` is the raw response
/// PHP wrote — it has its own header section (HTTP-shaped) followed by
/// a blank line and then the body. The caller is responsible for
/// splitting headers from body and merging them with bougie's
/// `Content-Type` etc.
pub async fn dispatch(
    socket: &Path,
    params: &[(&str, &str)],
    stdin: &[u8],
) -> Result<DispatchResult> {
    let mut stream = UnixStream::connect(socket)
        .await
        .wrap_err_with(|| format!("connecting to {}", socket.display()))?;

    // Buffer the entire outbound message before writing — keeps the
    // socket interaction tight and gives the kernel a chance to send
    // everything in one syscall.
    let mut tx = Vec::with_capacity(4096 + stdin.len());
    write_begin_request(&mut tx);
    write_record(&mut tx, RecordType::Params, &encode_params(params));
    write_record(&mut tx, RecordType::Params, &[]); // PARAMS terminator
    write_record(&mut tx, RecordType::Stdin, stdin);
    write_record(&mut tx, RecordType::Stdin, &[]); // STDIN terminator
    stream
        .write_all(&tx)
        .await
        .wrap_err("writing fastcgi request")?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let app_status: u32;

    loop {
        let frame = read_frame(&mut stream).await?;
        match frame.kind {
            RecordType::Stdout => {
                if !frame.body.is_empty() {
                    stdout.extend_from_slice(&frame.body);
                }
                // Empty STDOUT record = "end of stream" sentinel; keep
                // reading until we see END_REQUEST.
            }
            RecordType::Stderr => {
                if !frame.body.is_empty() {
                    stderr.extend_from_slice(&frame.body);
                }
            }
            RecordType::EndRequest => {
                if frame.body.len() < 8 {
                    return Err(eyre!("short END_REQUEST body: {} bytes", frame.body.len()));
                }
                app_status = u32::from_be_bytes([
                    frame.body[0],
                    frame.body[1],
                    frame.body[2],
                    frame.body[3],
                ]);
                let protocol_status = frame.body[4];
                if protocol_status != 0 {
                    return Err(eyre!(
                        "fastcgi protocol_status={protocol_status} (non-zero means the responder rejected the request)"
                    ));
                }
                break;
            }
            other => {
                return Err(eyre!("unexpected fastcgi record from responder: {other:?}"));
            }
        }
    }
    Ok(DispatchResult { stdout, stderr, app_status })
}

#[derive(Debug)]
pub struct DispatchResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub app_status: u32,
}

/// Probe a freshly-spawned pool by issuing FCGI_GET_VALUES and parsing
/// the response. 2s timeout (SERVER.md §7.6). The caller treats any
/// error here as "pool failed to start" and returns 502 to the client.
pub async fn probe(socket: &Path) -> Result<Vec<(String, String)>> {
    tokio::time::timeout(HEALTH_PROBE_TIMEOUT, probe_inner(socket))
        .await
        .map_err(|_| eyre!("fastcgi probe timed out after {:?}", HEALTH_PROBE_TIMEOUT))?
}

async fn probe_inner(socket: &Path) -> Result<Vec<(String, String)>> {
    let mut stream = UnixStream::connect(socket)
        .await
        .wrap_err_with(|| format!("connecting to {}", socket.display()))?;
    let mut tx = Vec::with_capacity(64);
    // FCGI_GET_VALUES requests well-known variables; an empty-valued
    // pair is the spec'd way to ask "what's your X?".
    write_record(
        &mut tx,
        RecordType::GetValues,
        &encode_params(&[
            ("FCGI_MAX_CONNS", ""),
            ("FCGI_MAX_REQS", ""),
            ("FCGI_MPXS_CONNS", ""),
        ]),
    );
    stream.write_all(&tx).await.wrap_err("writing GET_VALUES")?;
    // Reader: one frame is enough; well-behaved servers answer with a
    // single GetValuesResult.
    let frame = read_frame(&mut stream).await?;
    if !matches!(frame.kind, RecordType::GetValuesResult) {
        return Err(eyre!(
            "unexpected FastCGI record during health probe: {:?}",
            frame.kind
        ));
    }
    decode_params(&frame.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_short_pair_uses_one_byte_lengths() {
        let bytes = encode_params(&[("PATH_INFO", "/foo")]);
        assert_eq!(bytes[0], 9);   // PATH_INFO is 9 chars
        assert_eq!(bytes[1], 4);   // /foo is 4 chars
        assert_eq!(&bytes[2..11], b"PATH_INFO");
        assert_eq!(&bytes[11..15], b"/foo");
        assert_eq!(bytes.len(), 15);
    }

    #[test]
    fn encode_long_value_uses_four_byte_length() {
        let v = "x".repeat(200);
        let bytes = encode_params(&[("A", v.as_str())]);
        assert_eq!(bytes[0], 1);   // name len fits in 1 byte
        // value len 200 → top bit set + 4-byte big-endian
        assert_eq!(bytes[1] & 0x80, 0x80);
        let len = (u32::from(bytes[1] & 0x7f) << 24)
            | (u32::from(bytes[2]) << 16)
            | (u32::from(bytes[3]) << 8)
            | u32::from(bytes[4]);
        assert_eq!(len, 200);
    }

    #[test]
    fn encode_decode_round_trip() {
        let original = [
            ("REQUEST_METHOD".to_string(), "GET".to_string()),
            ("SCRIPT_FILENAME".to_string(), "/var/www/index.php".to_string()),
            ("HTTP_X_CUSTOM".to_string(), "1".to_string()),
        ];
        let pairs: Vec<(&str, &str)> = original
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let bytes = encode_params(&pairs);
        let decoded = decode_params(&bytes).unwrap();
        let decoded_pairs: Vec<(String, String)> = decoded.into_iter().collect();
        assert_eq!(decoded_pairs, original.to_vec());
    }

    #[test]
    fn long_pair_round_trips() {
        let big = "x".repeat(5000);
        let pairs = [("BIG", big.as_str())];
        let bytes = encode_params(&pairs);
        let decoded = decode_params(&bytes).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].0, "BIG");
        assert_eq!(decoded[0].1.len(), 5000);
    }

    #[test]
    fn write_record_fragments_at_64k() {
        let body = vec![0xa5_u8; 130_000];
        let mut buf = Vec::new();
        write_record(&mut buf, RecordType::Stdin, &body);
        // Two frames: 65535 + 64465. Each carries an 8-byte header.
        assert_eq!(buf.len(), FCGI_HEADER_LEN + 65_535 + FCGI_HEADER_LEN + 64_465);
        // First header opcodes + lengths.
        assert_eq!(buf[0], FCGI_VERSION_1);
        assert_eq!(buf[1], RecordType::Stdin as u8);
        let len1 = u16::from_be_bytes([buf[4], buf[5]]);
        assert_eq!(len1, 65_535);
    }

    #[test]
    fn write_empty_record_emits_header_only() {
        let mut buf = Vec::new();
        write_record(&mut buf, RecordType::Params, &[]);
        assert_eq!(buf.len(), FCGI_HEADER_LEN);
        let len = u16::from_be_bytes([buf[4], buf[5]]);
        assert_eq!(len, 0);
    }

    #[test]
    fn decode_rejects_truncation() {
        // Length byte says 10 chars, body has only 4.
        let bad = vec![10u8, 4u8, b'A', b'B'];
        assert!(decode_params(&bad).is_err());
    }
}
