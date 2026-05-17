//! `composer.json` reader.
//!
//! Bougie consumes `require.php`, the `require.ext-*` keys (presence
//! enables the extension; the value is ignored — see CLI.md §4.1), and
//! the optional `extra.bougie` block.

use super::BougieConfig;
use eyre::{Result, WrapErr};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerJson {
    pub require_php: Option<String>,
    /// Extension names with the `ext-` prefix stripped.
    pub require_extensions: BTreeSet<String>,
    pub extra_bougie: Option<BougieConfig>,
    /// `scripts` block, normalised: every entry is a non-empty list of
    /// shell-command strings. composer.json allows either a bare
    /// string (single step) or an array (run in order); both
    /// collapse to the array form here. Entries that aren't strings
    /// or string arrays — composer's `@scriptname` references, PHP
    /// callables, event listeners — are dropped: bougie's runner is
    /// a thin shell wrapper, not a composer-event reimplementation.
    pub scripts: BTreeMap<String, Vec<String>>,
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

    let scripts = v
        .get("scripts")
        .and_then(serde_json::Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(name, val)| normalise_script(val).map(|steps| (name.clone(), steps)))
                .collect()
        })
        .unwrap_or_default();

    Ok(ComposerJson {
        require_php,
        require_extensions,
        extra_bougie,
        scripts,
    })
}

/// Collapse a composer.json `scripts.<name>` value to a `Vec<String>`
/// of shell commands. Returns `None` for shapes bougie doesn't
/// execute (PHP callables, composer-event refs, non-string array
/// entries) — the caller treats those as "no script defined" and
/// falls through to its normal exec path.
fn normalise_script(v: &serde_json::Value) -> Option<Vec<String>> {
    match v {
        serde_json::Value::String(s) => Some(vec![s.clone()]),
        serde_json::Value::Array(items) => {
            let steps: Vec<String> = items
                .iter()
                .filter_map(|i| i.as_str().map(String::from))
                .collect();
            // Reject the mixed-types case: if the array had any
            // non-string entries (a PHP callable, say), running only
            // the shell-string subset would silently drop steps and
            // give the user a surprising partial execution.
            if steps.len() != items.len() || steps.is_empty() {
                return None;
            }
            Some(steps)
        }
        _ => None,
    }
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
        assert!(c.scripts.is_empty());
    }

    #[test]
    fn scripts_string_form_normalises_to_single_step() {
        let c = read_composer_json(r#"{"scripts":{"test":"phpunit"}}"#).unwrap();
        assert_eq!(c.scripts.get("test"), Some(&vec!["phpunit".to_string()]));
    }

    #[test]
    fn scripts_array_form_preserves_order() {
        let c = read_composer_json(
            r#"{"scripts":{"check":["phpcs","phpstan analyse"]}}"#,
        )
        .unwrap();
        assert_eq!(
            c.scripts.get("check"),
            Some(&vec!["phpcs".to_string(), "phpstan analyse".to_string()])
        );
    }

    #[test]
    fn scripts_drop_non_string_array_entries_wholesale() {
        // If an array has any non-string entry (e.g. composer's
        // event-listener object form), bougie drops the whole script:
        // executing only the shell-string subset would silently skip
        // steps and give a surprising partial run.
        let c = read_composer_json(
            r#"{"scripts":{
                "mixed":["phpunit",{"object":"with","fields":1}]
            }}"#,
        )
        .unwrap();
        assert!(!c.scripts.contains_key("mixed"));
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
