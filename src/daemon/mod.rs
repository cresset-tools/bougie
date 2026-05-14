//! `bougied` — the per-user service supervisor daemon.
//!
//! Same binary as `bougie`, dispatched via `argv[0] == "bougied"`
//! through `src/shim.rs`. The CLI auto-spawns it on the first
//! `bougie services …` invocation; subsequent commands reuse the
//! running daemon over a Unix socket at `$BOUGIE_HOME/state/bougied.sock`.
//!
//! Wire format: line-delimited JSON, schema-versioned, mirroring the
//! existing `bougie server` control socket
//! (`src/commands/server/control.rs`).
//!
//! Implementation lands across Phases 1–3 of the service-supervisor
//! work (see SERVICES.md in the php-build-standalone repo): this
//! module's body is stubbed until Phase 1's daemon skeleton lands.

use crate::Paths;
use eyre::{eyre, Result};
use std::process::ExitCode;

/// Entry point for the `bougied` argv[0] role.
///
/// Phase 1 will replace this stub with the tokio runtime + IPC
/// dispatcher described in the plan.
pub fn run(_paths: Paths) -> Result<ExitCode> {
    Err(eyre!(
        "bougied: not yet implemented (service supervisor lands in a follow-up commit)"
    ))
}
