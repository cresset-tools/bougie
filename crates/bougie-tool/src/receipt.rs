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
    #[serde(default)]
    pub with: Vec<String>,

    /// Denormalised hot-path field. `tool-exec` reads this and execs
    /// directly — no env lookup, no install-tree walk. Refreshed by
    /// `bougie php upgrade` (Phase 2 wiring).
    pub php_resolved_path: PathBuf,

    pub entrypoints: Vec<ToolEntrypoint>,
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
}
