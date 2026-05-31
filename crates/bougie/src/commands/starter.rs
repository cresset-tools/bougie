//! The bougie **starter-pack protocol** — `bougie init --starter <url|alias>`.
//!
//! A *starter* is a small JSON manifest a project-generator serves (e.g.
//! mageos-maker at `mageos-maker.cresset.tools`). `bougie init --starter`
//! fetches it and scaffolds a new project from it — primarily the
//! project's `composer.json`, plus optional hints. The shape is
//! deliberately minimal and framework-neutral so non-Magento tools
//! (Laravel/Symfony starters) can serve the same manifest.
//!
//! Manifest (schema 1):
//! ```json
//! {
//!   "schema": 1,
//!   "name": "Mage-OS Community 3.0.0 (Luma)",
//!   "composer-json": { "require": { ... }, "repositories": [ ... ] },
//!   "services": ["mariadb", "redis", "opensearch", "rabbitmq"],
//!   "recipe": "magento",
//!   "notes": ["Hyvä themes need a license token in auth.json"]
//! }
//! ```
//! Only `schema` and `composer-json` are required; the rest are optional
//! hints (`services`/`recipe` inform `--start`; `notes` are shown to the
//! user). bougie's own recipe auto-detection already covers Mage-OS, so
//! the hints are advisory, not load-bearing.

use eyre::{Result, WrapErr, eyre};
use serde::Deserialize;

/// The only manifest schema this bougie understands.
pub(crate) const SUPPORTED_SCHEMA: u32 = 1;

/// A parsed starter manifest. `composer_json` is written to the new
/// project's `composer.json`; the other fields are optional hints.
#[derive(Debug, Deserialize)]
pub(crate) struct StarterManifest {
    pub(crate) schema: u32,
    #[serde(rename = "composer-json")]
    pub(crate) composer_json: serde_json::Value,
    #[serde(default)]
    pub(crate) notes: Vec<String>,
    // Reserved protocol hints: parsed for forward-compat but not yet
    // consumed (bougie's recipe auto-detection already covers the
    // Mage-OS case; `name` is informational). Kept so the struct
    // documents the full manifest contract.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) services: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) recipe: Option<String>,
}

/// Resolve a built-in alias to a URL. A bare alias (`mageos`) is sugar
/// for the canonical maker URL; anything else is returned verbatim and
/// validated as a URL by [`fetch`].
fn resolve(starter: &str) -> &str {
    match starter {
        "mageos" | "mage-os" => "https://mageos-maker.cresset.tools/starter.json",
        other => other,
    }
}

/// Fetch + validate a starter manifest from a URL or built-in alias.
pub(crate) fn fetch(starter: &str) -> Result<StarterManifest> {
    let url = resolve(starter);
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(eyre!(
            "starter `{starter}` is neither a known alias (e.g. `mageos`) nor an http(s) URL"
        ));
    }

    let client = bougie_fetch::default_client()?;
    let resp = client
        .get(url)
        .send()
        .wrap_err_with(|| format!("fetching starter from {url}"))?;
    if !resp.status().is_success() {
        return Err(eyre!("fetching starter from {url}: HTTP {}", resp.status()));
    }
    let manifest: StarterManifest = resp
        .json()
        .wrap_err_with(|| format!("parsing starter manifest from {url}"))?;

    if manifest.schema != SUPPORTED_SCHEMA {
        return Err(eyre!(
            "starter uses manifest schema {} but this bougie understands schema {SUPPORTED_SCHEMA} \
             — upgrade bougie (`bougie self update`)",
            manifest.schema
        ));
    }
    if !manifest.composer_json.is_object() {
        return Err(eyre!("starter manifest `composer-json` must be a JSON object"));
    }
    Ok(manifest)
}

/// Render the manifest's `composer.json` as pretty JSON + trailing
/// newline. Key order is preserved (`serde_json` `preserve_order`), so the
/// generator's field ordering survives.
pub(crate) fn render_composer_json(manifest: &StarterManifest) -> String {
    let mut s =
        serde_json::to_string_pretty(&manifest.composer_json).expect("composer-json is a Value");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<StarterManifest> {
        let m: StarterManifest = serde_json::from_str(json)?;
        Ok(m)
    }

    #[test]
    fn parses_minimal_manifest() {
        let m = parse(r#"{"schema":1,"composer-json":{"require":{"php":"^8.4"}}}"#).unwrap();
        assert_eq!(m.schema, 1);
        assert!(m.services.is_empty());
        assert!(m.composer_json.is_object());
    }

    #[test]
    fn renders_composer_json_preserving_order() {
        let m = parse(
            r#"{"schema":1,"name":"x","composer-json":{"require":{"php":"^8.4"},"repositories":[]}}"#,
        )
        .unwrap();
        let out = render_composer_json(&m);
        // `require` appears before `repositories` (insertion order kept).
        assert!(out.find("require").unwrap() < out.find("repositories").unwrap());
        assert!(out.ends_with("}\n"));
    }

    #[test]
    fn alias_resolves_else_verbatim() {
        assert!(resolve("mageos").starts_with("https://"));
        assert_eq!(resolve("https://example/x.json"), "https://example/x.json");
        assert_eq!(resolve("./local"), "./local"); // rejected later by fetch()
    }
}
