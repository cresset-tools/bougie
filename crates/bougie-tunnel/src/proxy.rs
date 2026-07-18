//! Accept relay-opened substreams and splice each to the local server.
//!
//! This is the whole client-side proxy: the relay opens one yamux substream per
//! inbound browser request and speaks HTTP over it; we just pipe each substream
//! to a fresh loopback connection to the local `bougie server`. No HTTP parsing.

use std::future::poll_fn;

use eyre::Result;
use tokio::net::TcpStream;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::Mux;

/// Drive the yamux session: for each inbound substream, splice it to the local
/// server. Returns `Ok(())` when the relay closes the session.
pub(crate) async fn serve(mut conn: Mux, local_port: u16) -> Result<()> {
    loop {
        match poll_fn(|cx| conn.poll_next_inbound(cx)).await {
            Some(Ok(stream)) => {
                tokio::spawn(async move {
                    if let Err(e) = splice(stream, local_port).await {
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
async fn splice(stream: yamux::Stream, local_port: u16) -> Result<()> {
    let mut downstream = stream.compat(); // yamux Stream (futures-io) -> tokio-io
    let mut upstream = TcpStream::connect(("127.0.0.1", local_port)).await?;
    tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await?;
    Ok(())
}
