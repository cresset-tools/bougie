//! PHP interpreter selection for tools.
//!
//! Phase 1 picks the highest installed **NTS** PHP from the bougie
//! install tree. No index fetch, no auto-install — if no NTS PHP is
//! installed the caller is asked to `bougie php install` one. `--php`
//! pinning lands in Phase 2.
//!
//! NTS is the right default for CLI tools: tools like phpstan and
//! php-cs-fixer don't need threads, and many extensions only ship
//! NTS-compiled binaries.

use bougie_fs::store;
use bougie_paths::Paths;
use bougie_semver::version::Version;
use eyre::{Result, bail};
use std::path::PathBuf;

/// The interpreter a tool will be pinned to.
#[derive(Debug, Clone)]
pub struct PhpChoice {
    pub version: String,
    pub flavor: String,
    /// Path to the `php` binary itself, ready to drop into the
    /// receipt's `php_resolved_path`.
    pub bin: PathBuf,
}

/// Highest installed NTS PHP, or an error explaining how to install one.
pub fn pick_php(paths: &Paths) -> Result<PhpChoice> {
    let installed = store::list_installed(paths)
        .map_err(|e| eyre::eyre!("listing installed PHPs: {e}"))?;

    let mut candidates: Vec<(Version, String)> = installed
        .into_iter()
        .filter(|(_, flavor)| flavor == "nts")
        .filter_map(|(v, f)| Version::parse(&v).ok().map(|parsed| (parsed, f)))
        .collect();
    if candidates.is_empty() {
        bail!(
            "no NTS PHP installed. Install one with `bougie php install <version>` \
             (e.g. `bougie php install 8.3`)."
        );
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    // Length is checked above; `pop` after `is_empty` guarantees Some.
    let Some((version, flavor)) = candidates.pop() else {
        unreachable!("candidates checked non-empty")
    };
    let version_str = version.to_string();
    let bin = paths
        .installs()
        .join(format!("{version_str}-{flavor}"))
        .join("bin")
        .join("php");
    Ok(PhpChoice {
        version: version_str,
        flavor,
        bin,
    })
}
