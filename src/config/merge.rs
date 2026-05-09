//! §4.2.1 merge between `bougie.toml` and `composer.json`'s `extra.bougie`,
//! plus the loader that orchestrates reading both files from disk.

use super::{read_bougie_toml, BougieConfig, ComposerJson, IndexEntry, PhpConfig};
use eyre::{Result, WrapErr};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ProjectConfig {
    pub composer: Option<ComposerJson>,
    pub bougie: BougieConfig,
}

/// Merge per §4.2.1: `bougie.toml` wins per top-level key; tables are
/// deep-merged; arrays (`[[index]]`) replace wholesale.
pub fn merge(toml_cfg: BougieConfig, extra_cfg: BougieConfig) -> BougieConfig {
    BougieConfig {
        php: PhpConfig {
            version: toml_cfg.php.version.or(extra_cfg.php.version),
            flavor: toml_cfg.php.flavor.or(extra_cfg.php.flavor),
        },
        extensions: deep_merge_extensions(extra_cfg.extensions, toml_cfg.extensions),
        index: replace_if_nonempty(extra_cfg.index, toml_cfg.index),
    }
}

fn deep_merge_extensions(
    base: BTreeMap<String, String>,
    over: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = base;
    out.extend(over);
    out
}

fn replace_if_nonempty(base: Vec<IndexEntry>, over: Vec<IndexEntry>) -> Vec<IndexEntry> {
    if over.is_empty() {
        base
    } else {
        over
    }
}

/// Load both config sources from disk (each optional), merge, and
/// return both the original composer view and the merged bougie config.
pub fn load_project(project_root: &Path) -> Result<ProjectConfig> {
    let composer_path = project_root.join("composer.json");
    let composer = if composer_path.exists() {
        let text = std::fs::read_to_string(&composer_path)
            .wrap_err_with(|| format!("reading {}", composer_path.display()))?;
        Some(super::read_composer_json(&text)?)
    } else {
        None
    };

    let toml_path = project_root.join("bougie.toml");
    let toml_cfg = if toml_path.exists() {
        let text = std::fs::read_to_string(&toml_path)
            .wrap_err_with(|| format!("reading {}", toml_path.display()))?;
        read_bougie_toml(&text)?
    } else {
        BougieConfig::default()
    };

    let extra_cfg = composer
        .as_ref()
        .and_then(|c| c.extra_bougie.clone())
        .unwrap_or_default();

    Ok(ProjectConfig {
        composer,
        bougie: merge(toml_cfg, extra_cfg),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_php_version(v: &str) -> BougieConfig {
        BougieConfig {
            php: PhpConfig { version: Some(v.into()), flavor: None },
            ..Default::default()
        }
    }

    #[test]
    fn toml_scalar_wins_over_extra() {
        let toml_cfg = cfg_with_php_version("8.3.12");
        let extra_cfg = cfg_with_php_version("8.2.0");
        let merged = merge(toml_cfg, extra_cfg);
        assert_eq!(merged.php.version.as_deref(), Some("8.3.12"));
    }

    #[test]
    fn unset_in_toml_falls_back_to_extra() {
        let toml_cfg = BougieConfig::default();
        let extra_cfg = cfg_with_php_version("8.2.0");
        let merged = merge(toml_cfg, extra_cfg);
        assert_eq!(merged.php.version.as_deref(), Some("8.2.0"));
    }

    #[test]
    fn extension_tables_deep_merge() {
        let mut toml_exts = BTreeMap::new();
        toml_exts.insert("xdebug".into(), "3.5.1".into());
        let toml_cfg = BougieConfig {
            extensions: toml_exts,
            ..Default::default()
        };
        let mut extra_exts = BTreeMap::new();
        extra_exts.insert("redis".into(), "6.0.2".into());
        extra_exts.insert("xdebug".into(), "3.0.0".into()); // shadowed by toml
        let extra_cfg = BougieConfig {
            extensions: extra_exts,
            ..Default::default()
        };
        let merged = merge(toml_cfg, extra_cfg);
        assert_eq!(merged.extensions.len(), 2);
        assert_eq!(merged.extensions.get("xdebug").map(String::as_str), Some("3.5.1"));
        assert_eq!(merged.extensions.get("redis").map(String::as_str), Some("6.0.2"));
    }

    #[test]
    fn index_array_replaces_wholesale() {
        let toml_cfg = BougieConfig {
            index: vec![IndexEntry { host: "https://t".into(), fingerprint: "sha256:t".into() }],
            ..Default::default()
        };
        let extra_cfg = BougieConfig {
            index: vec![
                IndexEntry { host: "https://e1".into(), fingerprint: "sha256:e1".into() },
                IndexEntry { host: "https://e2".into(), fingerprint: "sha256:e2".into() },
            ],
            ..Default::default()
        };
        let merged = merge(toml_cfg, extra_cfg);
        assert_eq!(merged.index.len(), 1);
        assert_eq!(merged.index[0].host, "https://t");
    }

    #[test]
    fn empty_toml_index_falls_back_to_extra() {
        let toml_cfg = BougieConfig::default();
        let extra_cfg = BougieConfig {
            index: vec![IndexEntry { host: "https://e".into(), fingerprint: "sha256:e".into() }],
            ..Default::default()
        };
        let merged = merge(toml_cfg, extra_cfg);
        assert_eq!(merged.index.len(), 1);
        assert_eq!(merged.index[0].host, "https://e");
    }
}
