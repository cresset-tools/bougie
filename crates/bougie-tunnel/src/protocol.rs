//! Wire protocol between the tunnel client (laptop) and the relay.
//!
//! Re-exported from the shared [`bougie_tunnel_protocol`] crate — the single
//! source of truth for the client↔relay contract, so this end and the (closed)
//! relay can't drift. Kept as a `protocol` module here purely so existing
//! `crate::protocol::…` paths keep resolving.

pub use bougie_tunnel_protocol::*;
