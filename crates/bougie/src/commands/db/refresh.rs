//! `bougie db refresh` — pull the latest production snapshot and reload it
//! (`db pull` + `db seed --force`). The explicit "give me fresh prod data now"
//! action: unlike the one-shot `db seed`, it deliberately clobbers local DB
//! state with the newest production-shaped data (and updates the seed marker).
//! Because that discards local changes, an already-seeded database confirms
//! first (skip with `--yes`) — and it confirms *before* the pull, so nobody
//! downloads a multi-GB dump only to abort at the prompt.

use std::process::ExitCode;

use bougie_cli::{DbRefreshArgs, DbSeedArgs, OutputFormat};
use bougie_paths::Paths;
use eyre::Result;

use super::super::service::config_mut::locate_project_root;

pub fn run(format: OutputFormat, args: DbRefreshArgs) -> Result<ExitCode> {
    // The drift guard, up front: refreshing an already-seeded database replaces
    // whatever changed locally since. Never silent — confirm or `--yes`.
    if !args.yes {
        let paths = Paths::from_env()?;
        let project_root = locate_project_root()?;
        let marker_path = super::seed::seed_marker_path(&paths, &project_root);
        if let Some(marker) = super::seed::read_seed_marker(&marker_path) {
            if !super::seed::confirm_reseed(format, &marker)? {
                eprintln!("aborted; the database was left untouched.");
                return Ok(ExitCode::SUCCESS);
            }
        }
    }

    // Pull — this refreshes the per-project pulled-snapshot pointer. A pull
    // failure is an `Err` (propagated here), so we never reseed on a failed
    // download and clobber good local data with nothing.
    super::pull::run(format, args.pull)?;

    // Force-reseed from the just-pulled snapshot, bypassing the one-shot gate.
    // `yes: true`: the clobber was confirmed above (or `--yes` skipped it), so
    // seed must not prompt a second time.
    super::seed::run(
        format,
        DbSeedArgs {
            from: None,
            clean: false,
            force: true,
            yes: true,
        },
    )
}
