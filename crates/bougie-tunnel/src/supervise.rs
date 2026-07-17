//! Reconnect supervisor: redial with capped backoff, carrying the resume token
//! so the public URL survives a dropped connection.

use std::time::Duration;

use crate::{ShareHandle, TunnelClient};

/// Backoff schedule (seconds), capped — mirrors the daemon supervisor's
/// exponential-with-cap posture.
const BACKOFF: &[u64] = &[1, 2, 5, 10, 30];

/// Open the tunnel and keep it open across drops. `on_ready` is invoked with the
/// (possibly refreshed) [`ShareHandle`] each time a connection is established —
/// the first call is the one the `bougie share` command prints.
///
/// Never returns on its own; the caller wraps it in a `tokio::select!` against
/// `ctrl_c()` to end the share.
pub async fn run_forever<F>(mut client: TunnelClient, mut on_ready: F) -> !
where
    F: FnMut(&ShareHandle),
{
    let mut attempt = 0usize;
    loop {
        match client.open().await {
            Ok((handle, serving)) => {
                attempt = 0;
                // Carry the resume token so we keep the same URL next time.
                if handle.resume.is_some() {
                    let mut cfg = client.config().clone();
                    handle.resume.clone_into(&mut cfg.resume);
                    client = TunnelClient::new(cfg);
                }
                on_ready(&handle);
                if let Err(e) = serving.run().await {
                    tracing::warn!("tunnel dropped: {e}");
                }
            }
            Err(e) => tracing::warn!("relay connect failed: {e}"),
        }
        let secs = BACKOFF[attempt.min(BACKOFF.len() - 1)];
        attempt += 1;
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }
}
