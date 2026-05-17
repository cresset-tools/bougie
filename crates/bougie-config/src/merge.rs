//! §4.2.1 merge between `bougie.toml` and `composer.json`'s `extra.bougie`,
//! plus the loader that orchestrates reading both files from disk.

use super::{
    read_bougie_toml, BougieConfig, ComposerConfig, ComposerJson, IndexEntry, PhpConfig,
    ServerConfig,
};
#[cfg(test)]
use super::{ExtensionPin, ServicePin};
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
        composer: ComposerConfig {
            version: toml_cfg.composer.version.or(extra_cfg.composer.version),
        },
        extensions: deep_merge_map(extra_cfg.extensions, toml_cfg.extensions),
        services: deep_merge_map(extra_cfg.services, toml_cfg.services),
        index: replace_if_nonempty(extra_cfg.index, toml_cfg.index),
        server: ServerConfig {
            root: toml_cfg.server.root.or(extra_cfg.server.root),
        },
    }
}

fn deep_merge_map<V>(
    base: BTreeMap<String, V>,
    over: BTreeMap<String, V>,
) -> BTreeMap<String, V> {
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
        toml_exts.insert("xdebug".into(), ExtensionPin::Version("3.5.1".into()));
        let toml_cfg = BougieConfig {
            extensions: toml_exts,
            ..Default::default()
        };
        let mut extra_exts = BTreeMap::new();
        extra_exts.insert("redis".into(), ExtensionPin::Version("6.0.2".into()));
        extra_exts.insert("xdebug".into(), ExtensionPin::Version("3.0.0".into())); // shadowed by toml
        let extra_cfg = BougieConfig {
            extensions: extra_exts,
            ..Default::default()
        };
        let merged = merge(toml_cfg, extra_cfg);
        assert_eq!(merged.extensions.len(), 2);
        assert_eq!(merged.extensions.get("xdebug").and_then(ExtensionPin::as_version), Some("3.5.1"));
        assert_eq!(merged.extensions.get("redis").and_then(ExtensionPin::as_version), Some("6.0.2"));
    }

    #[test]
    fn toml_disabled_shadows_extra_version() {
        // mysqli = false in bougie.toml must take precedence over a
        // version pin in extra.bougie — otherwise the project can't
        // opt out of a baseline extension that an upstream `extra`
        // tried to pin.
        let mut toml_exts = BTreeMap::new();
        toml_exts.insert("mysqli".into(), ExtensionPin::Disabled(false));
        let mut extra_exts = BTreeMap::new();
        extra_exts.insert("mysqli".into(), ExtensionPin::Version("ignored".into()));
        let merged = merge(
            BougieConfig { extensions: toml_exts, ..Default::default() },
            BougieConfig { extensions: extra_exts, ..Default::default() },
        );
        assert!(merged.extensions.get("mysqli").unwrap().is_disabled());
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
    fn composer_version_toml_wins_over_extra() {
        let toml_cfg = BougieConfig {
            composer: ComposerConfig { version: Some("2.8.5".into()) },
            ..Default::default()
        };
        let extra_cfg = BougieConfig {
            composer: ComposerConfig { version: Some("2.7.0".into()) },
            ..Default::default()
        };
        let merged = merge(toml_cfg, extra_cfg);
        assert_eq!(merged.composer.version.as_deref(), Some("2.8.5"));
    }

    #[test]
    fn composer_version_unset_in_toml_falls_back_to_extra() {
        let toml_cfg = BougieConfig::default();
        let extra_cfg = BougieConfig {
            composer: ComposerConfig { version: Some("2.7.0".into()) },
            ..Default::default()
        };
        let merged = merge(toml_cfg, extra_cfg);
        assert_eq!(merged.composer.version.as_deref(), Some("2.7.0"));
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

    // -------------------- services merge --------------------

    #[test]
    fn service_tables_deep_merge_with_toml_winning() {
        let mut toml_svcs = BTreeMap::new();
        toml_svcs.insert("redis".into(), ServicePin::Version("8.6".into()));
        let mut extra_svcs = BTreeMap::new();
        extra_svcs.insert("mariadb".into(), ServicePin::Version("11.4".into()));
        // Shadowed by toml:
        extra_svcs.insert("redis".into(), ServicePin::Version("7.4".into()));
        let merged = merge(
            BougieConfig { services: toml_svcs, ..Default::default() },
            BougieConfig { services: extra_svcs, ..Default::default() },
        );
        assert_eq!(merged.services.len(), 2);
        assert_eq!(merged.services.get("redis").and_then(ServicePin::version), Some("8.6"));
        assert_eq!(merged.services.get("mariadb").and_then(ServicePin::version), Some("11.4"));
    }

    // -------------------- server merge --------------------

    #[test]
    fn server_root_toml_wins_over_extra() {
        let toml_cfg = BougieConfig {
            server: ServerConfig { root: Some("pub".into()) },
            ..Default::default()
        };
        let extra_cfg = BougieConfig {
            server: ServerConfig { root: Some("web".into()) },
            ..Default::default()
        };
        let merged = merge(toml_cfg, extra_cfg);
        assert_eq!(merged.server.root.as_deref(), Some("pub"));
    }

    #[test]
    fn server_root_unset_in_toml_falls_back_to_extra() {
        let toml_cfg = BougieConfig::default();
        let extra_cfg = BougieConfig {
            server: ServerConfig { root: Some("public".into()) },
            ..Default::default()
        };
        let merged = merge(toml_cfg, extra_cfg);
        assert_eq!(merged.server.root.as_deref(), Some("public"));
    }

    #[test]
    fn service_detail_overrides_extra_bare_pin() {
        // bougie.toml `[services.mariadb] version = "11.4"; tenant = "foo"`
        // wins over `extra.bougie.services.mariadb = "10.6"`.
        let mut toml_svcs = BTreeMap::new();
        toml_svcs.insert(
            "mariadb".into(),
            ServicePin::Detail(super::super::ServicePinDetail {
                version: Some("11.4".into()),
                tenant: Some("foo".into()),
            }),
        );
        let mut extra_svcs = BTreeMap::new();
        extra_svcs.insert("mariadb".into(), ServicePin::Version("10.6".into()));
        let merged = merge(
            BougieConfig { services: toml_svcs, ..Default::default() },
            BougieConfig { services: extra_svcs, ..Default::default() },
        );
        let m = merged.services.get("mariadb").unwrap();
        assert_eq!(m.version(), Some("11.4"));
        assert_eq!(m.tenant(), Some("foo"));
    }
}
