//! Per-tool bookkeeping file at `$TOOL_DIR/receipt.toml`.
//!
//! Written at install, mutated on inject / upgrade (Phase 2+), deleted
//! on uninstall. The receipt is load-bearing at runtime: the `tool-exec`
//! shim reads `php_resolved_path` and `package` from here to set up the
//! exec, so the format is stable across phases and serde structs gain
//! optional fields rather than getting rewritten.
//!
//! Phase 1 ships with `with` always empty and no plugin list; later
//! phases populate them.

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolReceipt {
    pub package: String,
    pub constraint: String,
    pub php_version: String,
    pub php_flavor: String,
    pub composer_version: String,
    /// Composer packages added via `--with` / `inject`. Vendor/name
    /// form; constraint suffix preserved (e.g.
    /// `phpstan/phpstan-strict-rules@^1.5`).
    #[serde(default)]
    pub with: Vec<String>,

    /// Denormalised hot-path field. `tool-exec` reads this and execs
    /// directly — no env lookup, no install-tree walk. Refreshed by
    /// `bougie php upgrade`.
    pub php_resolved_path: PathBuf,

    pub entrypoints: Vec<ToolEntrypoint>,

    /// PHP extensions added via `--with` / `inject`. Forwards-compat
    /// with Phase 1 receipts (default = empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<ToolExtension>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExtension {
    pub name: String,
    /// Absolute path to the `$TOOL_DIR/conf.d/20-<name>.ini` fragment
    /// that loads the extension. Removed on uninject / uninstall.
    pub ini_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEntrypoint {
    /// Bin name as it appears on PATH.
    pub name: String,
    /// Absolute path of the symlink in `$BOUGIE_TOOL_BIN_DIR/`. What
    /// uninstall removes.
    pub install_path: PathBuf,
    /// Composer package the entry point came from. Almost always the
    /// tool's own package; differs only when a `--with` extra owns the
    /// bin (not allowed in Phase 1, but the field is stable).
    pub from: String,
}

pub fn read(path: &Path) -> Result<ToolReceipt> {
    let text = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("reading tool receipt {}", path.display()))?;
    toml_edit::de::from_str(&text)
        .wrap_err_with(|| format!("parsing tool receipt {}", path.display()))
}

pub fn write(path: &Path, receipt: &ToolReceipt) -> Result<()> {
    let text = toml_edit::ser::to_string_pretty(receipt)
        .wrap_err("serialising tool receipt")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, text)
        .wrap_err_with(|| format!("writing tool receipt {}", path.display()))
}

/// A single (old, new) PHP upgrade tuple. `bougie php upgrade` accumulates
/// one per interpreter that actually moved to a higher patch.
#[derive(Debug, Clone)]
pub struct PhpUpgrade {
    /// Resolved triplet *before* the upgrade — what receipts that
    /// pinned this PHP currently record in `php_version`.
    pub from_version: String,
    /// Resolved triplet *after* the upgrade.
    pub to_version: String,
    pub flavor: String,
    /// Absolute path to the new `php` binary; lands in receipts'
    /// `php_resolved_path`.
    pub new_bin: PathBuf,
}

/// Walk `paths.tools()/*/receipt.toml`. For any receipt whose
/// `(php_version, php_flavor)` matches one of `upgrades`, rewrite
/// `php_version` to the new triplet and `php_resolved_path` to the
/// new binary, then save.
///
/// Returns the receipt paths that were updated. Receipts that fail
/// to parse are surfaced as a stderr warning and skipped — one bad
/// receipt shouldn't block a global `bougie php upgrade`.
pub fn refresh_php_pin(
    paths: &bougie_paths::Paths,
    upgrades: &[PhpUpgrade],
) -> Result<Vec<PathBuf>> {
    let mut refreshed = Vec::new();
    if upgrades.is_empty() {
        return Ok(refreshed);
    }
    let root = paths.tools();
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(refreshed),
        Err(e) => return Err(eyre::Report::new(e).wrap_err(format!("reading {}", root.display()))),
    };
    for entry in entries.flatten() {
        let receipt_path = entry.path().join("receipt.toml");
        if !receipt_path.is_file() {
            continue;
        }
        let mut receipt = match read(&receipt_path) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "warning: skipping tool receipt {}: {e}",
                    receipt_path.display()
                );
                continue;
            }
        };
        let Some(upgrade) = upgrades.iter().find(|u| {
            u.from_version == receipt.php_version && u.flavor == receipt.php_flavor
        }) else {
            continue;
        };
        receipt.php_version = upgrade.to_version.clone();
        receipt.php_resolved_path = upgrade.new_bin.clone();
        write(&receipt_path, &receipt)?;
        refreshed.push(receipt_path);
    }
    refreshed.sort();
    Ok(refreshed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ToolReceipt {
        ToolReceipt {
            package: "phpstan/phpstan".into(),
            constraint: "^1.10".into(),
            php_version: "8.3.12".into(),
            php_flavor: "nts".into(),
            composer_version: "2.8.12".into(),
            with: vec![],
            php_resolved_path: PathBuf::from("/home/u/.local/share/bougie/installs/8.3.12-nts/bin/php"),
            entrypoints: vec![ToolEntrypoint {
                name: "phpstan".into(),
                install_path: PathBuf::from("/home/u/.local/bin/phpstan"),
                from: "phpstan/phpstan".into(),
            }],
            extensions: vec![],
        }
    }

    #[test]
    fn roundtrip_via_disk() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("receipt.toml");
        let r = sample();
        write(&path, &r).unwrap();
        let parsed = read(&path).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn missing_with_defaults_to_empty() {
        // Forwards-compat: an older receipt without `with` must still
        // load (Phase 1 callers never wrote it, but Phase 2 does).
        let text = r#"
package = "phpstan/phpstan"
constraint = "^1.10"
php_version = "8.3.12"
php_flavor = "nts"
composer_version = "2.8.12"
php_resolved_path = "/x/php"
entrypoints = []
"#;
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("receipt.toml");
        std::fs::write(&path, text).unwrap();
        let parsed = read(&path).unwrap();
        assert!(parsed.with.is_empty());
    }

    #[test]
    fn malformed_receipt_surfaces_path() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("receipt.toml");
        std::fs::write(&path, "this = is = not = toml").unwrap();
        let err = read(&path).unwrap_err().to_string();
        assert!(err.contains("receipt.toml"), "{err}");
    }

    fn paths_for(td: &Path) -> bougie_paths::Paths {
        bougie_paths::Paths::new(td.to_path_buf(), td.join("cache"))
    }

    fn write_sample_receipt(paths: &bougie_paths::Paths, pkg: &str, php_version: &str) -> PathBuf {
        let dir = paths.tools().join(pkg.replace('/', "-"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut r = sample();
        r.package = pkg.into();
        r.php_version = php_version.into();
        let receipt_path = dir.join("receipt.toml");
        write(&receipt_path, &r).unwrap();
        receipt_path
    }

    #[test]
    fn refresh_php_pin_updates_matching_receipts() {
        let td = tempfile::TempDir::new().unwrap();
        let paths = paths_for(td.path());
        let p1 = write_sample_receipt(&paths, "phpstan/phpstan", "8.3.12");
        let p2 = write_sample_receipt(&paths, "vimeo/psalm", "8.3.12");
        let p3 = write_sample_receipt(&paths, "rector/rector", "8.2.20");

        let new_bin = PathBuf::from("/installs/8.3.13-nts/bin/php");
        let upgrades = vec![PhpUpgrade {
            from_version: "8.3.12".into(),
            to_version: "8.3.13".into(),
            flavor: "nts".into(),
            new_bin: new_bin.clone(),
        }];
        let refreshed = refresh_php_pin(&paths, &upgrades).unwrap();
        assert_eq!(refreshed.len(), 2, "{refreshed:?}");
        assert!(refreshed.contains(&p1));
        assert!(refreshed.contains(&p2));

        let r1 = read(&p1).unwrap();
        assert_eq!(r1.php_version, "8.3.13");
        assert_eq!(r1.php_resolved_path, new_bin);
        let r3 = read(&p3).unwrap();
        assert_eq!(r3.php_version, "8.2.20", "non-matching receipt left alone");
    }

    #[test]
    fn refresh_php_pin_ignores_flavor_mismatch() {
        let td = tempfile::TempDir::new().unwrap();
        let paths = paths_for(td.path());
        let p1 = write_sample_receipt(&paths, "phpstan/phpstan", "8.3.12");
        // Receipt's php_flavor is "nts" (from sample()). Upgrade is
        // for zts — should not touch the receipt.
        let upgrades = vec![PhpUpgrade {
            from_version: "8.3.12".into(),
            to_version: "8.3.13".into(),
            flavor: "zts".into(),
            new_bin: PathBuf::from("/installs/8.3.13-zts/bin/php"),
        }];
        let refreshed = refresh_php_pin(&paths, &upgrades).unwrap();
        assert!(refreshed.is_empty());
        let r1 = read(&p1).unwrap();
        assert_eq!(r1.php_version, "8.3.12");
    }

    #[test]
    fn refresh_php_pin_empty_upgrades_is_noop() {
        let td = tempfile::TempDir::new().unwrap();
        let paths = paths_for(td.path());
        write_sample_receipt(&paths, "phpstan/phpstan", "8.3.12");
        let refreshed = refresh_php_pin(&paths, &[]).unwrap();
        assert!(refreshed.is_empty());
    }

    #[test]
    fn refresh_php_pin_handles_missing_tools_dir() {
        let td = tempfile::TempDir::new().unwrap();
        let paths = paths_for(td.path());
        let upgrades = vec![PhpUpgrade {
            from_version: "8.3.12".into(),
            to_version: "8.3.13".into(),
            flavor: "nts".into(),
            new_bin: PathBuf::from("/x"),
        }];
        let refreshed = refresh_php_pin(&paths, &upgrades).unwrap();
        assert!(refreshed.is_empty());
    }
}
