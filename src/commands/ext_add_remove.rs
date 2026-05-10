//! `bougie ext add` / `bougie ext remove` delegate to Composer per
//! CLI.md §3.2.1 / §3.2.2 — bougie does not edit composer.json
//! directly. After composer succeeds, an implicit `bougie sync` runs so
//! the project's `.bougie/conf.d` reflects the new ext set without a
//! second user step. Pass `--no-sync` to skip.
//!
//! Composer itself is provided by bougie via the project shim
//! (`.bougie/bin/composer`, populated by `bougie sync`). System composer
//! is no longer consulted.

use crate::cli::OutputFormat;
use crate::commands::sync::{ensure_synced, project_php_inputs};
use crate::config::load_project;
use crate::output::{emit, Render};
use crate::paths::Paths;
use eyre::{eyre, Result, WrapErr};
use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[derive(Debug, Serialize)]
pub struct ExtAddRemoveResult {
    pub schema_version: u32,
    pub action: &'static str,
    pub names: Vec<String>,
}

impl Render for ExtAddRemoveResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for n in &self.names {
            writeln!(w, "{} ext-{n}", self.action)?;
        }
        Ok(())
    }
}

pub fn add(
    format: OutputFormat,
    field: Option<&str>,
    names: Vec<String>,
    no_sync: bool,
) -> Result<ExitCode> {
    delegate("require", "add", names, format, field, no_sync)
}

pub fn remove(
    format: OutputFormat,
    field: Option<&str>,
    names: Vec<String>,
    no_sync: bool,
) -> Result<ExitCode> {
    delegate("remove", "remove", names, format, field, no_sync)
}

fn delegate(
    composer_verb: &str,
    action: &'static str,
    names: Vec<String>,
    format: OutputFormat,
    field: Option<&str>,
    no_sync: bool,
) -> Result<ExitCode> {
    if names.is_empty() {
        return Err(eyre!("no extensions specified"));
    }
    let project_root = locate_project_root()?;
    let composer_shim = locate_or_install_composer_shim(&project_root)?;
    let mut cmd = Command::new(&composer_shim);
    cmd.arg(composer_verb);
    for n in &names {
        cmd.arg(format!("ext-{n}"));
    }
    let status = cmd
        .status()
        .wrap_err_with(|| format!("invoking {}", composer_shim.display()))?;
    if !status.success() {
        return Err(eyre!("composer exited with status {status}"));
    }

    if !no_sync {
        // composer.json was just rewritten by composer; reload before
        // syncing so the new ext set drives the conf.d generation.
        let paths = Paths::from_env()?;
        let project = load_project(&project_root)?;
        let (spec, flavor) = project_php_inputs(&project)?;
        ensure_synced(&paths, &project_root, &project, spec, flavor)?;
    }

    let result = ExtAddRemoveResult { schema_version: 1, action, names };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn locate_project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    cwd.ancestors()
        .find(|p| p.join(".bougie").is_dir())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            eyre!(
                "no bougie project here (no `.bougie/` in {} or any parent) — \
                 run `bougie init` first",
                cwd.display()
            )
        })
}

/// Locate the project's composer shim, implicitly syncing the project if
/// the shim isn't there yet. `ext add`/`ext remove` triggers the
/// "on-demand install" half of the composer-management contract (the
/// "always on sync" half is in `ensure_synced` itself).
fn locate_or_install_composer_shim(project_root: &Path) -> Result<PathBuf> {
    let shim = project_root.join(".bougie").join("bin").join("composer");
    if !shim.exists() {
        eprintln!("Syncing… (run `bougie sync` to do this explicitly)");
        let paths = Paths::from_env()?;
        let project = load_project(project_root)?;
        let (spec, flavor) = project_php_inputs(&project)?;
        ensure_synced(&paths, project_root, &project, spec, flavor)?;
    }
    Ok(shim)
}
