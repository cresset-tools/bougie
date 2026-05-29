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
            match std::fs::symlink_metadata(&entry.install_path) {
                Ok(_) => {
                    std::fs::remove_file(&entry.install_path).wrap_err_with(|| {
                        format!("removing {}", entry.install_path.display())
                    })?;
                    removed_bins.push(entry.install_path.clone());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => bail!(
                    "checking {}: {e}",
                    entry.install_path.display()
                ),
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
