//! `bougie tool uninstall <vendor/name>` core logic.
//!
//! Receipt-driven: we delete only the `entrypoints[*].install_path`
//! files the receipt records, then `rm -rf` the tool dir. We never
//! scan the user's bin dir for "things we think we own" — without a
//! receipt entry, the file isn't ours.

use crate::receipt;
use bougie_fs::lock::ExclusiveGuard;
use bougie_paths::Paths;
use eyre::{Result, WrapErr, bail};
use std::path::PathBuf;
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_mins(1);

#[derive(Debug, Clone)]
pub struct UninstallOutcome {
    pub package: String,
    pub tool_dir: PathBuf,
    pub removed_bins: Vec<PathBuf>,
}

pub fn uninstall(paths: &Paths, package: &str) -> Result<UninstallOutcome> {
    let tool_dir = paths.tool_dir(package);
    if !tool_dir.exists() {
        bail!("tool `{package}` is not installed");
    }

    let guard = ExclusiveGuard::acquire(&tool_dir.join(".lock"), LOCK_TIMEOUT)
        .wrap_err_with(|| {
            format!(
                "acquiring lock on {} (is another `bougie tool` running?)",
                tool_dir.display()
            )
        })?;

    let receipt_path = tool_dir.join("receipt.toml");
    let mut removed_bins = Vec::new();
    if receipt_path.exists() {
        let receipt = receipt::read(&receipt_path)?;
        for entry in &receipt.entrypoints {
            // Only remove the PATH symlink if it still points into *this*
            // tool's dir. A later `bougie tool install --force` of a
            // different tool sharing a bin name re-points the symlink; the
            // receipt still records the path, but the file is no longer
            // ours, so deleting it would silently break the other tool.
            match std::fs::read_link(&entry.install_path) {
                Ok(target) if target.starts_with(&tool_dir) => {
                    std::fs::remove_file(&entry.install_path).wrap_err_with(|| {
                        format!("removing {}", entry.install_path.display())
                    })?;
                    removed_bins.push(entry.install_path.clone());
                }
                // Symlink now points elsewhere (reclaimed by another tool)
                // — leave it alone.
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // Not a symlink (e.g. replaced by a regular file we don't
                // own): don't clobber it.
                Err(_) => {}
            }
        }
    }

    // Drop the lock before `remove_dir_all` so its file handle doesn't
    // hold the parent open on Windows. On Unix this is a no-op.
    drop(guard);

    std::fs::remove_dir_all(&tool_dir)
        .wrap_err_with(|| format!("removing {}", tool_dir.display()))?;

    Ok(UninstallOutcome {
        package: package.to_string(),
        tool_dir,
        removed_bins,
    })
}
