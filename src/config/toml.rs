//! `bougie.toml` reader and skeleton writer.

use super::BougieConfig;
use eyre::{Result, WrapErr};

pub fn read_bougie_toml(text: &str) -> Result<BougieConfig> {
    toml_edit::de::from_str(text).wrap_err("parsing bougie.toml")
}

/// Skeleton emitted by `bougie init --toml`. Hand-written (not via
/// `toml_edit::Document::new() + serde`) so that comments survive
/// later round-trips through the same `toml_edit` document.
pub fn write_bougie_toml_skeleton() -> String {
    [
        "# bougie configuration. Both this file and composer.json's `extra.bougie`",
        "# block are first-class. See CLI.md §4.",
        "",
        "[php]",
        "# version = \"8.3.12\"     # optional override of composer.json's require.php",
        "# flavor = \"nts\"          # nts | nts-debug | zts | zts-debug",
        "",
        "[extensions]",
        "# xdebug = \"3.5.1\"",
        "",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_yields_defaults() {
        let cfg = read_bougie_toml("").unwrap();
        assert!(cfg.php.version.is_none());
        assert!(cfg.extensions.is_empty());
        assert!(cfg.index.is_empty());
    }

    #[test]
    fn full_config_roundtrips() {
        let text = r#"
[php]
version = "8.3.12"
flavor = "nts"

[extensions]
xdebug = "3.5.1"
redis = "6.0.2"

[[index]]
host = "https://i.example"
fingerprint = "sha256:abc"
"#;
        let cfg = read_bougie_toml(text).unwrap();
        assert_eq!(cfg.php.version.as_deref(), Some("8.3.12"));
        assert_eq!(cfg.extensions.len(), 2);
        assert_eq!(cfg.index.len(), 1);
    }

    #[test]
    fn skeleton_parses_back_to_empty() {
        let cfg = read_bougie_toml(&write_bougie_toml_skeleton()).unwrap();
        assert_eq!(cfg, BougieConfig::default());
    }
}
