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
//!   "placeholders": [
//!     {"token": "{{hyva_project}}", "prompt": "Hyvä project slug",
//!      "description": "Your Hyvä repo slug from hyva.io", "required": true}
//!   ],
//!   "notes": ["Hyvä themes need a license token in auth.json"]
//! }
//! ```
//! Only `schema` and `composer-json` are required; the rest are optional.
//!
//! `placeholders` lets a producer ship a *shared* manifest that still needs
//! per-user values it must not bake in (an account-identifying repo slug, an
//! org name). Each entry names a literal `token` left inside `composer-json`;
//! `bougie new --starter` prompts the user for each on a TTY and substitutes
//! their answer before writing `composer.json` (see [`resolve_placeholders`]).
//! `recipe` and `services` are persisted into the scaffolded project's
//! `extra.bougie` block (see [`apply_project_hints`]) so they're load-bearing
//! for `bougie start` — the producer can name the recipe explicitly rather than
//! relying on bougie's composer.json auto-detection. `notes` are shown to the
//! user; `name` is informational.

use eyre::{Result, WrapErr, eyre};
use serde::Deserialize;
use std::io::{self, Write};

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
    /// Service names to declare in the project (→ `extra.bougie.services`).
    #[serde(default)]
    pub(crate) services: Vec<String>,
    /// Builtin recipe to pin for the project (→ `extra.bougie.recipe`).
    #[serde(default)]
    pub(crate) recipe: Option<String>,
    /// Per-user values the producer left as literal tokens in `composer-json`
    /// (it must not bake them in). Resolved interactively before write.
    #[serde(default)]
    pub(crate) placeholders: Vec<Placeholder>,
    // Informational only.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) name: Option<String>,
}

/// One manifest placeholder: a literal `token` somewhere in `composer-json`
/// that bougie asks the user to fill in. The producer leaves a token (rather
/// than a real value) when the value is per-user and must not be shared — e.g.
/// an account-identifying Hyvä repo slug.
#[derive(Debug, Deserialize)]
pub(crate) struct Placeholder {
    /// The exact string occurring in `composer-json` to substitute.
    pub(crate) token: String,
    /// Short question shown at the prompt. Falls back to `token` if absent.
    #[serde(default)]
    pub(crate) prompt: Option<String>,
    /// Human-readable explanation of what the value is, printed above the
    /// prompt so the user knows what they're entering.
    #[serde(default)]
    pub(crate) description: Option<String>,
    /// Value used when the user just presses enter (and in non-interactive
    /// runs). When absent and `required`, a non-interactive run is an error.
    #[serde(default)]
    pub(crate) default: Option<String>,
    /// Whether an empty answer is rejected. Defaults to true.
    #[serde(default = "default_true")]
    pub(crate) required: bool,
}

fn default_true() -> bool {
    true
}

/// Persist the manifest's optional `recipe` / `services` hints into the
/// scaffolded `composer.json`'s `extra.bougie` block, so `bougie start` honours
/// them (recipe selection + service bring-up) instead of treating them as
/// advisory. The producer (e.g. mageos-maker) can thus name the recipe
/// explicitly rather than relying on composer.json auto-detection.
pub(crate) fn apply_project_hints(
    composer_json: &mut serde_json::Value,
    recipe: Option<&str>,
    services: &[String],
) {
    if recipe.is_none() && services.is_empty() {
        return;
    }
    let Some(root) = composer_json.as_object_mut() else { return };
    let extra = root
        .entry("extra")
        .or_insert_with(|| serde_json::json!({}));
    let Some(extra) = extra.as_object_mut() else { return };
    let bougie = extra
        .entry("bougie")
        .or_insert_with(|| serde_json::json!({}));
    let Some(bougie) = bougie.as_object_mut() else { return };

    if let Some(recipe) = recipe {
        bougie.insert("recipe".to_string(), serde_json::Value::String(recipe.to_string()));
    }
    if !services.is_empty() {
        let svc = bougie
            .entry("services")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(svc) = svc.as_object_mut() {
            for name in services {
                svc.entry(name.clone())
                    .or_insert_with(|| serde_json::Value::String("*".to_string()));
            }
        }
    }
}

/// Prompt the user for each manifest placeholder (or apply defaults) and
/// substitute their answers into `composer_json` in place.
///
/// `interactive` gates whether stdin may be read (a TTY + text output). When
/// it's false, a `required` placeholder with no `default` is a hard error
/// rather than silently leaving an unresolved token (e.g. an invalid Hyvä repo
/// URL) in the scaffolded project. Prompts and hints go to stderr so
/// `--format json-v1` keeps a clean stdout.
pub(crate) fn resolve_placeholders(
    composer_json: &mut serde_json::Value,
    placeholders: &[Placeholder],
    interactive: bool,
) -> Result<()> {
    for p in placeholders {
        let label = p.prompt.as_deref().unwrap_or(&p.token);
        let value = if interactive {
            prompt_placeholder(label, p.description.as_deref(), p.default.as_deref(), p.required)?
        } else if let Some(def) = &p.default {
            Some(def.clone())
        } else if p.required {
            return Err(eyre!(
                "this starter needs a value for `{label}` (token `{}`), but input isn't \
                 interactive — re-run `bougie new --starter …` in a terminal",
                p.token
            ));
        } else {
            None
        };
        if let Some(value) = value {
            substitute_token(composer_json, &p.token, &value);
        }
    }
    Ok(())
}

/// Ask once for a single placeholder, looping until a non-empty answer when
/// `required` (an empty line accepts `default` if there is one). Returns `None`
/// only for an optional placeholder the user left blank.
fn prompt_placeholder(
    label: &str,
    description: Option<&str>,
    default: Option<&str>,
    required: bool,
) -> Result<Option<String>> {
    if let Some(description) = description {
        eprintln!("{description}");
    }
    loop {
        match default {
            Some(def) => eprint!("{label} [{def}]: "),
            None => eprint!("{label}: "),
        }
        io::stderr().flush().ok();

        let mut line = String::new();
        let read = io::stdin()
            .read_line(&mut line)
            .map_err(|e| eyre!("reading input for `{label}`: {e}"))?;
        let ans = line.trim();

        if !ans.is_empty() {
            return Ok(Some(ans.to_string()));
        }
        // Empty line (or EOF, read == 0): take the default if any.
        if let Some(def) = default {
            return Ok(Some(def.to_string()));
        }
        if !required {
            return Ok(None);
        }
        if read == 0 {
            // stdin closed with nothing to give for a required value.
            return Err(eyre!("no input provided for required value `{label}`"));
        }
        eprintln!("a value is required.");
    }
}

/// Replace every occurrence of `token` inside string values of `value`
/// (recursively). Operating on the JSON tree rather than the rendered text
/// keeps replacements correctly escaped no matter what the user typed.
fn substitute_token(value: &mut serde_json::Value, token: &str, replacement: &str) {
    match value {
        serde_json::Value::String(s) => {
            *s = s.replace(token, replacement);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                substitute_token(item, token, replacement);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                substitute_token(v, token, replacement);
            }
        }
        _ => {}
    }
}

/// Resolve a `--starter` value to the manifest URL to fetch.
///
/// - A built-in alias (`mageos`/`mage-os`) → the canonical maker manifest
///   URL.
/// - A URL pointing directly at a manifest (ending in `.json`) → used
///   verbatim.
/// - Any other http(s) URL is treated as a **starter base** and bougie
///   appends `/starter.json`. This is the protocol convention — a starter
///   server serves its manifest at `<base>/starter.json` — and it means
///   the URL you can copy from a browser works as-is: the maker's site
///   root (`…/`) and its per-config share link (`…/c/{id}`, an HTML page)
///   both resolve to the matching `…/starter.json` endpoint.
fn manifest_url(starter: &str) -> Result<String> {
    if matches!(starter, "mageos" | "mage-os") {
        return Ok("https://mageos-maker.cresset.tools/starter.json".to_string());
    }
    if !(starter.starts_with("https://") || starter.starts_with("http://")) {
        return Err(eyre!(
            "starter `{starter}` is neither a known alias (e.g. `mageos`) nor an http(s) URL"
        ));
    }
    let base = starter.trim_end_matches('/');
    let is_manifest = std::path::Path::new(base)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
    if is_manifest {
        Ok(base.to_string())
    } else {
        Ok(format!("{base}/starter.json"))
    }
}

/// Fetch + validate a starter manifest from a URL or built-in alias.
pub(crate) fn fetch(starter: &str) -> Result<StarterManifest> {
    let url = manifest_url(starter)?;

    let client = bougie_fetch::default_client()?;
    let resp = client
        .get(url.as_str())
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
    if let Some(p) = manifest.placeholders.iter().find(|p| p.token.is_empty()) {
        return Err(eyre!(
            "starter manifest has a placeholder with an empty `token` (prompt: {:?})",
            p.prompt
        ));
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
    fn manifest_url_alias() {
        assert_eq!(
            manifest_url("mageos").unwrap(),
            "https://mageos-maker.cresset.tools/starter.json"
        );
        assert_eq!(
            manifest_url("mage-os").unwrap(),
            "https://mageos-maker.cresset.tools/starter.json"
        );
    }

    #[test]
    fn manifest_url_direct_json_is_verbatim() {
        assert_eq!(
            manifest_url("https://example/x.json").unwrap(),
            "https://example/x.json"
        );
        // A trailing slash is trimmed but a `.json` target is still used as-is.
        assert_eq!(
            manifest_url("https://example/starter.json/").unwrap(),
            "https://example/starter.json"
        );
    }

    #[test]
    fn manifest_url_base_gets_starter_json_appended() {
        // The maker's per-config share link (an HTML page) → its manifest.
        assert_eq!(
            manifest_url("https://mageos-maker.cresset.tools/c/abc-123").unwrap(),
            "https://mageos-maker.cresset.tools/c/abc-123/starter.json"
        );
        // Site root, with or without a trailing slash.
        assert_eq!(
            manifest_url("https://mageos-maker.cresset.tools/").unwrap(),
            "https://mageos-maker.cresset.tools/starter.json"
        );
        assert_eq!(
            manifest_url("https://example.com").unwrap(),
            "https://example.com/starter.json"
        );
    }

    #[test]
    fn manifest_url_rejects_non_url() {
        assert!(manifest_url("./local").is_err());
    }

    #[test]
    fn apply_project_hints_writes_recipe_and_services() {
        let mut composer = serde_json::json!({"require": {"php": "^8.4"}});
        apply_project_hints(&mut composer, Some("magento"), &["mariadb".into(), "opensearch".into()]);

        assert_eq!(composer["extra"]["bougie"]["recipe"], "magento");
        assert_eq!(composer["extra"]["bougie"]["services"]["mariadb"], "*");
        assert_eq!(composer["extra"]["bougie"]["services"]["opensearch"], "*");
        // Pre-existing keys are preserved.
        assert_eq!(composer["require"]["php"], "^8.4");
    }

    #[test]
    fn apply_project_hints_is_a_noop_without_hints() {
        let mut composer = serde_json::json!({"require": {}});
        apply_project_hints(&mut composer, None, &[]);
        assert!(composer.get("extra").is_none());
    }

    #[test]
    fn parses_placeholders_with_defaults() {
        let m = parse(
            r#"{"schema":1,"composer-json":{"require":{}},
                "placeholders":[
                  {"token":"{{slug}}","prompt":"Slug","description":"a hint","required":true},
                  {"token":"{{org}}"}
                ]}"#,
        )
        .unwrap();
        assert_eq!(m.placeholders.len(), 2);
        assert_eq!(m.placeholders[0].token, "{{slug}}");
        assert_eq!(m.placeholders[0].prompt.as_deref(), Some("Slug"));
        assert!(m.placeholders[0].required);
        // `required` defaults to true when omitted.
        assert!(m.placeholders[1].required);
        assert!(m.placeholders[1].prompt.is_none());
    }

    #[test]
    fn substitute_token_replaces_in_nested_strings_only() {
        let mut v = serde_json::json!({
            "repositories": [
                {"type": "composer", "url": "https://h.example/{{slug}}/"}
            ],
            "require": {"php": "^8.4"},
            "count": 3
        });
        substitute_token(&mut v, "{{slug}}", "my-org-abc");
        assert_eq!(v["repositories"][0]["url"], "https://h.example/my-org-abc/");
        // Non-string scalars and unrelated strings are untouched.
        assert_eq!(v["count"], 3);
        assert_eq!(v["require"]["php"], "^8.4");
    }

    #[test]
    fn substitute_token_escapes_special_chars() {
        // A replacement containing a quote must survive as valid JSON once
        // rendered, because we substitute into the Value, not the text.
        let mut v = serde_json::json!({"name": "{{x}}"});
        substitute_token(&mut v, "{{x}}", "a\"b\\c");
        let rendered = serde_json::to_string(&v).unwrap();
        let round: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(round["name"], "a\"b\\c");
    }

    #[test]
    fn resolve_placeholders_uses_defaults_non_interactive() {
        let mut composer = serde_json::json!({"url": "{{slug}}"});
        let placeholders = vec![Placeholder {
            token: "{{slug}}".into(),
            prompt: Some("Slug".into()),
            description: None,
            default: Some("fallback".into()),
            required: true,
        }];
        resolve_placeholders(&mut composer, &placeholders, false).unwrap();
        assert_eq!(composer["url"], "fallback");
    }

    #[test]
    fn resolve_placeholders_errors_on_required_without_default_non_interactive() {
        let mut composer = serde_json::json!({"url": "{{slug}}"});
        let placeholders = vec![Placeholder {
            token: "{{slug}}".into(),
            prompt: Some("Slug".into()),
            description: None,
            default: None,
            required: true,
        }];
        let err = resolve_placeholders(&mut composer, &placeholders, false).unwrap_err();
        assert!(err.to_string().contains("interactive"));
        // Token left untouched on error.
        assert_eq!(composer["url"], "{{slug}}");
    }

    #[test]
    fn resolve_placeholders_skips_optional_blank_non_interactive() {
        let mut composer = serde_json::json!({"url": "{{slug}}"});
        let placeholders = vec![Placeholder {
            token: "{{slug}}".into(),
            prompt: None,
            description: None,
            default: None,
            required: false,
        }];
        resolve_placeholders(&mut composer, &placeholders, false).unwrap();
        // Optional + no default + non-interactive → token left in place.
        assert_eq!(composer["url"], "{{slug}}");
    }

    #[test]
    fn fetch_rejects_empty_placeholder_token() {
        // Parsing alone is fine; the empty-token check lives in `fetch`, so
        // assert it via the same predicate fetch uses.
        let m = parse(
            r#"{"schema":1,"composer-json":{"require":{}},"placeholders":[{"token":""}]}"#,
        )
        .unwrap();
        assert!(m.placeholders.iter().any(|p| p.token.is_empty()));
    }

    #[test]
    fn manifest_parses_recipe_and_services() {
        let m = parse(
            r#"{"schema":1,"composer-json":{"require":{}},"recipe":"magento","services":["mariadb"]}"#,
        )
        .unwrap();
        assert_eq!(m.recipe.as_deref(), Some("magento"));
        assert_eq!(m.services, vec!["mariadb".to_string()]);
    }
}
