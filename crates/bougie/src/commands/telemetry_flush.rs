//! `bougie __telemetry-flush` — the hidden, detached upload child.
//!
//! Spawned with null stdio by `Recorder::maybe_spawn_flush` after a
//! user command finishes; never runs in-band. Deprioritizes itself
//! before touching anything (nice 19 / autogroup nice on Linux; the
//! Windows spawner sets `BELOW_NORMAL_PRIORITY_CLASS` instead).

use bougie_paths::Paths;
use eyre::Result;
use std::process::ExitCode;

pub fn run() -> Result<ExitCode> {
    bougie_telemetry::flush::deprioritize();
    let paths = Paths::from_env()?;
    let stats = bougie_telemetry::flush::run_flush(&paths, env!("CARGO_PKG_VERSION"))?;
    tracing::debug!(
        "telemetry flush: {} file(s), {} event(s), {} byte(s)",
        stats.files,
        stats.events,
        stats.bytes
    );
    Ok(ExitCode::SUCCESS)
}
