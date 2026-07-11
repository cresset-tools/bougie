//! `bougie db refresh` — pull the latest production snapshot and reload it
//! (`db pull` + `db seed --force`). The explicit "give me fresh prod data now"
//! action: unlike the one-shot `db seed`, it deliberately clobbers local DB
//! state with the newest production-shaped data (and updates the seed marker).

use std::process::ExitCode;

use bougie_cli::{DbPullArgs, DbSeedArgs, OutputFormat};
use eyre::Result;

pub fn run(format: OutputFormat, args: DbPullArgs) -> Result<ExitCode> {
    // Pull first — this refreshes the per-project pulled-snapshot pointer. A
    // pull failure is an `Err` (propagated here), so we never reseed on a failed
    // download and clobber good local data with nothing.
    super::pull::run(format, args)?;

    // Force-reseed from the just-pulled snapshot, bypassing the one-shot gate.
    super::seed::run(
        format,
        DbSeedArgs {
            from: None,
            clean: false,
            force: true,
        },
    )
}
