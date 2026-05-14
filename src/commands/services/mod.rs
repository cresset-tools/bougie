//! `bougie services …` — the client-side subcommands.
//!
//! Most of this surface is a thin IPC client over `bougied`. The
//! exception is `catalog` (lands in Phase 2), which is a pure read of
//! the built-in catalog and needs no running daemon.

pub mod client;
pub mod daemon;
