//! `bougie tool upgrade <pkg>` / `--all` / `--reinstall`.
//!
//! Two flows:
//!
//! - **Non-reinstall** keeps the tool dir, regenerates `composer.json`
//!   from the receipt, re-resolves the lock (which pulls fresh
//!   Packagist metadata), re-runs `install_from_lock`, and refreshes
//!   the bin wrappers. Bin entrypoints whose names disappeared between
//!   versions are pruned from PATH; new ones land via the normal
//!   wrapper-emission path.
//! - **`--reinstall`** snapshots the receipt's `(package, constraint,
//!   php_version, with, extensions)` tuple, wipes the tool dir + every
//!   PATH symlink the old receipt listed, then calls back into
//!   `install::install` with the snapshot. Useful as a recovery for
//!   broken state.
//!
//! `--all` walks every installed tool, calls `upgrade_one` per tool,
//! and aggregates outcomes — per-tool failures are reported but don't
//! stop the loop.

use crate::install::{InstallContext, install};
use crate::list::{ListedTool, ToolStatus, list};
use crate::receipt::{self, ToolEntrypoint};
use crate::request::ToolRequest;
use crate::{install as install_mod, uninstall};
use bougie_composer_resolver::{InstallOptions, install_from_lock};
use bougie_fs::lock::ExclusiveGuard;
use eyre::{Result, WrapErr, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_mins(1);

#[derive(Debug, Clone)]
pub struct UpgradeOutcome {
    pub package: String,
    pub tool_dir: PathBuf,
    pub previous_php: String,
    pub current_php: String,
    pub installed_bins: Vec<PathBuf>,
    pub reinstalled: bool,
}

pub fn upgrade_one(
    ctx: &InstallContext<'_>,
    package: &str,
    reinstall: bool,
) -> Result<UpgradeOutcome> {
    let tool_dir = ctx.paths.tool_dir(package);
    if !tool_dir.exists() {
        bail!("tool `{package}` is not installed");
    }

    if reinstall {
        return reinstall_one(ctx, package, &tool_dir);
    }

    let guard = ExclusiveGuard::acquire(&tool_dir.join(".lock"), LOCK_TIMEOUT)
        .wrap_err_with(|| {
            format!("acquiring lock on {} (is another `bougie tool` running?)", tool_dir.display())
        })?;

    let mut receipt = receipt::read(&tool_dir.join("receipt.toml"))?;
    let previous_php = receipt.php_version.clone();

    install_mod::write_composer_json_for_receipt(&tool_dir, &receipt)?;
    (ctx.resolve_lock)(ctx.paths, &tool_dir)
        .wrap_err("resolving composer.lock for tool")?;
    install_from_lock(ctx.paths, &tool_dir, InstallOptions { no_dev: true })
        .wrap_err("installing tool dependencies during upgrade")?;

    // Rebuild bin wrappers. New bin set might differ across versions:
    // names that disappeared get pruned from PATH; new names get
    // symlinked (with `force = true` so a name carried across
    // versions is overwritten in place).
    let (entrypoints, installed_bins) =
        install_mod::emit_bins(ctx.paths, &tool_dir, &receipt.package, true)?;
    prune_dropped_entrypoints(&receipt.entrypoints, &entrypoints)?;
    receipt.entrypoints = entrypoints;
    receipt::write(&tool_dir.join("receipt.toml"), &receipt)?;
    drop(guard);

    Ok(UpgradeOutcome {
        package: package.to_string(),
        tool_dir,
        previous_php,
        current_php: receipt.php_version,
        installed_bins,
        reinstalled: false,
    })
}

fn reinstall_one(
    ctx: &InstallContext<'_>,
    package: &str,
    tool_dir: &Path,
) -> Result<UpgradeOutcome> {
    // Snapshot first so the wipe + re-install can rebuild against the
    // user's original choices. PHP version is pinned to the exact
    // recorded triplet (`--php 8.3.13`) — auto-install triggers if
    // it's no longer on disk after `bougie php upgrade`.
    let receipt_path = tool_dir.join("receipt.toml");
    let snapshot = receipt::read(&receipt_path)
        .wrap_err_with(|| format!("reading {} for --reinstall", receipt_path.display()))?;
    let previous_php = snapshot.php_version.clone();

    uninstall::uninstall(ctx.paths, package)?;

    let request = ToolRequest {
        vendor: snapshot
            .package
            .split_once('/')
            .map_or_else(String::new, |(v, _)| v.to_string()),
        name: snapshot
            .package
            .split_once('/')
            .map_or_else(|| snapshot.package.clone(), |(_, n)| n.to_string()),
        constraint: Some(snapshot.constraint.clone()),
    };
    let php_spec = snapshot.php_version.clone();

    let outcome = install(
        ctx,
        &request,
        Some(&php_spec),
        &snapshot.with,
        true, // force — we just wiped, but the parent dir of the bin
              // symlinks may have user-placed files we own re-emitting.
    )?;

    Ok(UpgradeOutcome {
        package: outcome.package,
        tool_dir: outcome.tool_dir,
        previous_php,
        current_php: outcome.php_version,
        installed_bins: outcome.installed_bins,
        reinstalled: true,
    })
}

/// Delete PATH symlinks for entrypoints the upgrade dropped — names
/// that were in the old receipt but aren't in the new one. The new
/// entrypoints' own symlinks were already (re-)written by
/// `emit_bins` with `force = true`.
fn prune_dropped_entrypoints(
    old: &[ToolEntrypoint],
    new: &[ToolEntrypoint],
) -> Result<()> {
    for old_ep in old {
        if new.iter().any(|n| n.name == old_ep.name) {
            continue;
        }
        match std::fs::symlink_metadata(&old_ep.install_path) {
            Ok(_) => std::fs::remove_file(&old_ep.install_path).wrap_err_with(|| {
                format!("removing dropped entrypoint {}", old_ep.install_path.display())
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => bail!("checking {}: {e}", old_ep.install_path.display()),
        }
    }
    Ok(())
}

/// Walk every installed tool and call `upgrade_one` per package.
/// Per-tool failures are collected so a single broken tool doesn't
/// abort the rest of the upgrade.
pub fn upgrade_all(
    ctx: &InstallContext<'_>,
    reinstall: bool,
) -> Result<Vec<(String, Result<UpgradeOutcome>)>> {
    let mut out = Vec::new();
    for tool in list(ctx.paths)? {
        let ListedTool { status, .. } = &tool;
        let package = match status {
            ToolStatus::Healthy(r) | ToolStatus::Stale { receipt: r, .. } => r.package.clone(),
            ToolStatus::Broken { .. } => continue, // can't upgrade what we can't parse
        };
        let result = upgrade_one(ctx, &package, reinstall);
        out.push((package, result));
    }
    Ok(out)
}

