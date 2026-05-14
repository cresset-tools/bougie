//! `composer.json` reader.
//!
//! Bougie consumes `require.php`, the `require.ext-*` keys (presence
//! enables the extension; the value is ignored — see CLI.md §4.1), and
//! the optional `extra.bougie` block.

use super::BougieConfig;
use eyre::{Result, WrapErr};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerJson {
    pub require_php: Option<String>,
    /// Extension names with the `ext-` prefix stripped.
    pub require_extensions: BTreeSet<String>,
    pub extra_bougie: Option<BougieConfig>,
}

pub fn read_composer_json(text: &str) -> Result<ComposerJson> {
    let v: serde_json::Value = serde_json::from_str(text).wrap_err("parsing composer.json")?;
    let require = v.get("require").and_then(serde_json::Value::as_object);

    let require_php = require
        .and_then(|r| r.get("php"))
        .and_then(serde_json::Value::as_str)
        .map(String::from);

    let require_extensions = require
        .map(|r| {
            r.keys()
                .filter_map(|k| k.strip_prefix("ext-").map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let extra_bougie = v
        .get("extra")
        .and_then(|e| e.get("bougie"))
        .map(|b| serde_json::from_value::<BougieConfig>(b.clone()))
        .transpose()
        .wrap_err("deserializing extra.bougie")?;

    Ok(ComposerJson { require_php, require_extensions, extra_bougie })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_composer_yields_defaults() {
        let c = read_composer_json("{}").unwrap();
        assert_eq!(c.require_php, None);
        assert!(c.require_extensions.is_empty());
        assert!(c.extra_bougie.is_none());
    }

    #[test]
    fn require_php_and_ext_keys_are_extracted() {
        let c = read_composer_json(
            r#"{"require":{"php":"^8.3","ext-xdebug":"*","ext-redis":"*"}}"#,
        )
        .unwrap();
        assert_eq!(c.require_php.as_deref(), Some("^8.3"));
        assert!(c.require_extensions.contains("xdebug"));
        assert!(c.require_extensions.contains("redis"));
    }

    #[test]
    fn non_ext_require_keys_are_ignored() {
        let c = read_composer_json(
            r#"{"require":{"php":"^8.3","monolog/monolog":"^3.0"}}"#,
        )
        .unwrap();
        assert_eq!(c.require_extensions.len(), 0);
    }

    #[test]
    fn extra_bougie_is_parsed() {
        let c = read_composer_json(
            r#"{
                "extra": {
                    "bougie": {
                        "php": {"version": "8.3.12", "flavor": "nts"},
                        "extensions": {"xdebug": "3.5.1"},
                        "index": [{"host": "https://i.example", "fingerprint": "sha256:abc"}]
                    }
                }
            }"#,
        )
        .unwrap();
        let cfg = c.extra_bougie.unwrap();
        assert_eq!(cfg.php.version.as_deref(), Some("8.3.12"));
        assert_eq!(cfg.php.flavor.as_deref(), Some("nts"));
        assert_eq!(
            cfg.extensions
                .get("xdebug")
                .and_then(crate::config::ExtensionPin::as_version),
            Some("3.5.1")
        );
        assert_eq!(cfg.index.len(), 1);
        assert_eq!(cfg.index[0].host, "https://i.example");
    }

    #[test]
    fn extension_can_be_disabled_via_false_sentinel() {
        // composer.json's `extra.bougie.extensions` accepts `false` to
        // opt a baseline extension out of the project's auto-enable
        // set (CLI.md §3.3 step 4).
        let c = read_composer_json(
            r#"{
                "extra": {
                    "bougie": {
                        "extensions": {"mysqli": false, "redis": "6.0.2"}
                    }
                }
            }"#,
        )
        .unwrap();
        let cfg = c.extra_bougie.unwrap();
        assert!(cfg.extensions.get("mysqli").unwrap().is_disabled());
        assert_eq!(
            cfg.extensions
                .get("redis")
                .and_then(crate::config::ExtensionPin::as_version),
            Some("6.0.2")
        );
    }

    #[test]
    fn extra_bougie_composer_version_is_parsed() {
        let c = read_composer_json(
            r#"{
                "extra": {
                    "bougie": {
                        "composer": {"version": "2.8.5"}
                    }
                }
            }"#,
        )
        .unwrap();
        let cfg = c.extra_bougie.unwrap();
        assert_eq!(cfg.composer.version.as_deref(), Some("2.8.5"));
    }

    #[test]
    fn extra_without_bougie_block_is_none() {
        let c = read_composer_json(r#"{"extra":{"other":{"k":"v"}}}"#).unwrap();
        assert!(c.extra_bougie.is_none());
    }

    #[test]
    fn malformed_json_errors() {
        assert!(read_composer_json("{not json").is_err());
    }

    #[test]
    fn services_bare_string_pin_parses() {
        let c = read_composer_json(
            r#"{
                "extra": {
                    "bougie": {
                        "services": {"redis": "8.6", "mariadb": "*"}
                    }
                }
            }"#,
        )
        .unwrap();
        let cfg = c.extra_bougie.unwrap();
        assert_eq!(cfg.services.len(), 2);
        assert_eq!(
            cfg.services.get("redis").and_then(crate::config::ServicePin::version),
            Some("8.6")
        );
        assert_eq!(
            cfg.services.get("mariadb").and_then(crate::config::ServicePin::version),
            Some("*")
        );
    }

    #[test]
    fn services_table_form_parses_with_tenant() {
        let c = read_composer_json(
            r#"{
                "extra": {
                    "bougie": {
                        "services": {
                            "mariadb": {"version": "11.4", "tenant": "myapp"}
                        }
                    }
                }
            }"#,
        )
        .unwrap();
        let cfg = c.extra_bougie.unwrap();
        let m = cfg.services.get("mariadb").unwrap();
        assert_eq!(m.version(), Some("11.4"));
        assert_eq!(m.tenant(), Some("myapp"));
    }
}
