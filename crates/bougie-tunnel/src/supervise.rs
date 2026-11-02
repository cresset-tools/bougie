//! Reconnect supervisor: keep a share up across dropped connections.
//!
//! When the session ends — the relay closed it, or keepalive found the link
//! dead — redial with capped backoff, presenting the share's resume token so
//! the relay hands back the same URL. A relay that can't resume (it restarted,
//! or the grace period ran out) issues a new share instead; the caller learns
//! of it through [`Event::Reconnected`] and can re-register the new host.
//!
//! Only a *refusal* (bad login, not allowed to share) stops the loop: retrying
//! can't fix it. Network errors are retried for as long as the caller lets the
//! future run, and so is a relay that hands a `--slug` share a different host
//! ([`crate::SlugNotGranted`]): that share must not move to another URL.

use std::future::Future;
use std::time::Duration;

use eyre::Result;

use crate::{Refused, Serving, ShareHandle, TunnelClient};

/// Backoff schedule (seconds) between attempts, capped at the last entry.
const BACKOFF: &[u64] = &[1, 2, 5, 10, 30];

/// What happened to the share, for the caller to report.
#[derive(Debug, Clone)]
pub enum Event {
    /// The connection dropped; reconnecting.
    Lost { reason: String },
    /// An attempt failed; the next one is in `wait`.
    Retrying {
        attempt: usize,
        wait: Duration,
        error: String,
    },
    /// Connected again. `handle.host` differs from before if the relay could
    /// not resume the old share.
    Reconnected(ShareHandle),
}

/// Serve `first` (from [`TunnelClient::open`]) and keep the share up across
/// drops, reporting each change to `on_event`. Returns only when the relay
/// refuses a reconnect.
pub async fn keep_up<F, Fut>(
    mut client: TunnelClient,
    first: (ShareHandle, Serving),
    mut on_event: F,
) -> Result<()>
where
    F: FnMut(Event) -> Fut,
    Fut: Future<Output = ()>,
{
    let (mut handle, mut serving) = first;
    loop {
        client = client.resuming(&handle);
        let reason = match serving.run().await {
            Ok(()) => "the relay closed the connection".to_owned(),
            Err(e) => e.to_string(),
        };
        on_event(Event::Lost { reason }).await;

        let mut attempt = 0usize;
        (handle, serving) = loop {
            match client.open().await {
                Ok(opened) => break opened,
                Err(e) if e.downcast_ref::<Refused>().is_some() => return Err(e),
                Err(e) => {
                    let wait = Duration::from_secs(BACKOFF[attempt.min(BACKOFF.len() - 1)]);
                    attempt += 1;
                    on_event(Event::Retrying {
                        attempt,
                        wait,
                        error: e.to_string(),
                    })
                    .await;
                    tokio::time::sleep(wait).await;
                }
            }
        };
        on_event(Event::Reconnected(handle.clone())).await;
    }
}
