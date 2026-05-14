//! `bougie services …` — the client-side subcommands.
//!
//! Most of this surface is a thin IPC client over `bougied`. The
//! offline subcommands (`catalog`, `add`, `remove`, `list`) need no
//! running daemon.

pub mod add;
pub mod catalog;
pub mod client;
pub mod config_mut;
pub mod daemon;
pub mod down;
pub mod list;
pub mod remove;
pub mod status;
pub mod up;
