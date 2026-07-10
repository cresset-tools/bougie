//! `bougie start` — the project lifecycle umbrella. Brings the whole
//! project up by running the detected recipe's `start` task, whose DAG
//! already composes sync → services → setup → server (see the builtin
//! recipes and RECIPES.md). This is deliberately a thin wrapper over
//! `bougie make start`: the orchestration lives in the recipe, not here,
//! so `start` and `make start` can never drift.
//!
//! `bougie stop` is the teardown twin and lives inline in the dispatcher
//! (it's just `service down` for the project's declared services).

use crate::commands::make::{self, MakeOptions};
use bougie_cli::OutputFormat;
use eyre::{Result, WrapErr};
use std::path::Path;
use std::process::ExitCode;

/// Recipe-relevant knobs forwarded from `bougie start`. A subset of
/// [`MakeOptions`] — `start` always targets the `start` task and never
/// lists or prints.
#[derive(Debug, Default, Clone)]
#[allow(clippy::struct_excessive_bools, reason = "each is a distinct forwarded CLI flag")]
pub struct StartOptions {
    pub no_sync: bool,
    pub dry_run: bool,
    pub explain: bool,
    pub no_builtin: bool,
    pub recipe: Option<String>,
}

/// Run the project's `start` task in the current directory.
pub fn run(format: OutputFormat, opts: StartOptions) -> Result<ExitCode> {
    make::run(
        format,
        MakeOptions {
            task: Some("start".to_string()),
            dry_run: opts.dry_run,
            explain: opts.explain,
            no_sync: opts.no_sync,
            no_builtin: opts.no_builtin,
            recipe: opts.recipe,
            ..Default::default()
        },
    )
}

/// `--start` for `init` / `new`: enter the freshly-scaffolded project
/// root (a no-op for `init`, where root == cwd) and bring it up. The
/// single entry point so `init --start` and `bougie start` stay in step.
pub fn run_in(format: OutputFormat, root: &Path) -> Result<ExitCode> {
    std::env::set_current_dir(root)
        .wrap_err_with(|| format!("entering {}", root.display()))?;
    run(format, StartOptions::default())
}
