//! Laravel package discovery — native reimplementation.
//!
//! Laravel doesn't use a Composer *plugin*; it relies on a Composer
//! *script* (`post-autoload-dump` → `@php artisan package:discover`) plus
//! `Illuminate\Foundation\ComposerScripts::postAutoloadDump`. bougie runs
//! neither scripts nor plugins, so it reproduces their effect:
//!
//! 1. **Discovery** ([`build_package_manifest`] + [`render_packages_php`])
//!    — port of `Illuminate\Foundation\PackageManifest::build()`: read
//!    `vendor/composer/installed.json`, map each package to its
//!    `extra.laravel` config, honor `dont-discover`, and write
//!    `bootstrap/cache/packages.php` as `<?php return var_export(...);`.
//!
//! 2. **Cache invalidation** (the `clearCompiled()` half) — the caller
//!    deletes the stale `bootstrap/cache/{config,services}.php` caches.
//!    [`STALE_CACHES`] names them.
//!
//! The output matches PHP's `var_export($manifest, true)` byte-for-byte
//! (verified against PHP 8.4), so it's indistinguishable from what
//! `artisan package:discover` would write.

use serde_json::{Map, Value};

/// `bootstrap/cache` files that Laravel's `clearCompiled()` removes (in
/// addition to `packages.php`, which is then rebuilt). Project-relative.
/// Paths are overridable in Laravel via `APP_*_CACHE` env vars; bougie
/// uses the defaults.
pub const STALE_CACHES: [&str; 2] = ["bootstrap/cache/config.php", "bootstrap/cache/services.php"];

/// Project-relative path of the discovery manifest bougie writes.
pub const PACKAGES_CACHE: &str = "bootstrap/cache/packages.php";

/// Inspect a project's `composer.json` `scripts` object and return the
/// `post-autoload-dump` entries that **block** bougie from safely
/// reproducing Laravel's discovery — i.e. unknown steps that run *before*
/// or *between* the default Laravel steps. An empty result means it's
/// safe to reproduce the defaults.
///
/// bougie stands in for Laravel's `post-autoload-dump` script (it writes
/// `packages.php` and clears the compiled caches) instead of running it.
/// The default steps it reproduces are `artisan package:discover` and
/// `Illuminate\Foundation\ComposerScripts::postAutoloadDump`.
///
/// Position matters. A custom step *after* the defaults is harmless: the
/// defaults still run first exactly as bougie reproduces them, and the
/// trailing step is just an unrun custom script (covered by the generic
/// "scripts not run" warning). But a custom step *before* or *between* the
/// defaults could mutate state the defaults depend on, so bougie can't
/// vouch for reproducing them — those entries are returned so the caller
/// can refuse. If the script contains no recognizable default at all
/// (e.g. discovery was renamed/removed), every entry is returned; an
/// empty/absent script is safe (bougie just regenerates the manifest).
///
/// `scripts` is the `composer.json` `scripts` value (any non-object, or a
/// missing `post-autoload-dump`, yields an empty list).
#[must_use]
pub fn blocking_post_autoload_dump(scripts: &Value) -> Vec<String> {
    let entries: Vec<&str> = match scripts.get("post-autoload-dump") {
        // Composer allows a single string or an array of strings.
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => return Vec::new(),
    };

    match entries.iter().rposition(|e| is_reproduced_script(e)) {
        // Defaults present: only unknown steps at-or-before the last
        // default block reproduction. Everything after it is a trailing
        // extra and is allowed.
        Some(last_default) => entries[..last_default]
            .iter()
            .filter(|e| !is_reproduced_script(e))
            .map(|e| (*e).to_string())
            .collect(),
        // No recognizable default step. Empty/absent → safe (handled by
        // the early return above for the missing-key case; an empty array
        // yields an empty list here too). A non-empty script with no
        // default means discovery was replaced — block on all of it.
        None => entries.iter().map(|e| (*e).to_string()).collect(),
    }
}

/// Whether a single `post-autoload-dump` entry is one bougie reproduces.
fn is_reproduced_script(entry: &str) -> bool {
    // Collapse internal whitespace so flag spacing doesn't matter.
    let norm = entry.split_whitespace().collect::<Vec<_>>().join(" ");
    norm == "Illuminate\\Foundation\\ComposerScripts::postAutoloadDump"
        || norm == "@php artisan package:discover"
        // tolerate flags, e.g. `--ansi`
        || norm.starts_with("@php artisan package:discover ")
}

/// Build the package-discovery manifest from a parsed
/// `vendor/composer/installed.json` and the root `composer.json`'s
/// `extra` object. Returns a JSON object mapping `vendor/name` →
/// `extra.laravel` config, exactly as
/// `Illuminate\Foundation\PackageManifest::build()` assembles it:
///
/// - packages with no (or empty) `extra.laravel` are dropped;
/// - the root's `extra.laravel.dont-discover` plus any package-declared
///   `dont-discover` lists are excluded;
/// - `dont-discover: ["*"]` in the *root* drops everything.
#[must_use]
pub fn build_package_manifest(installed_json: &Value, root_extra: &Value) -> Value {
    // Composer 2 wraps the list in `{ "packages": [...] }`; Composer 1
    // wrote a bare array. Mirror PackageManifest's `$installed['packages']
    // ?? $installed`.
    let empty: Vec<Value> = Vec::new();
    let packages = installed_json
        .get("packages")
        .and_then(Value::as_array)
        .or_else(|| installed_json.as_array())
        .unwrap_or(&empty);

    // Seed ignore list from the root's dont-discover; `*` there ignores
    // everything (computed before package-declared lists are merged, so
    // only the root can trigger ignore-all — matching upstream).
    let mut ignore = dont_discover(root_extra.get("laravel"));
    let ignore_all = ignore.iter().any(|s| s == "*");

    // First pass: collect each package's laravel config and merge any
    // package-declared dont-discover into the ignore set.
    let mut mapped: Vec<(String, Value)> = Vec::new();
    for pkg in packages {
        let Some(name) = pkg.get("name").and_then(Value::as_str) else {
            continue;
        };
        let laravel = pkg
            .get("extra")
            .and_then(|e| e.get("laravel"))
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        for d in dont_discover(Some(&laravel)) {
            if !ignore.contains(&d) {
                ignore.push(d);
            }
        }
        mapped.push((name.to_string(), laravel));
    }

    // Second pass: drop ignored and empty-config packages.
    let mut out = Map::new();
    if !ignore_all {
        for (name, laravel) in mapped {
            if ignore.contains(&name) || is_falsy(&laravel) {
                continue;
            }
            out.insert(name, laravel);
        }
    }
    Value::Object(out)
}

/// Render a manifest as the PHP source Laravel writes to
/// `bootstrap/cache/packages.php`: `<?php return ` + PHP `var_export` of
/// the array + `;` (no trailing newline).
#[must_use]
pub fn render_packages_php(manifest: &Value) -> String {
    format!("<?php return {};", var_export(manifest, 0))
}

/// Read a `laravel.dont-discover` string list, tolerating absence.
fn dont_discover(laravel: Option<&Value>) -> Vec<String> {
    laravel
        .and_then(|l| l.get("dont-discover"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// PHP `filter()` (no callback) drops falsy values. An absent or empty
/// `extra.laravel` is `[]`, which is falsy → the package isn't discovered.
fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => true,
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::String(s) => s.is_empty(),
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Bool(true) => false,
    }
}

/// Port of PHP's `var_export($value, true)` for the JSON value shapes a
/// Laravel manifest contains. `indent` is the column at which an emitted
/// `array (` keyword (and its closing `)`) sit; scalars ignore it.
fn var_export(value: &Value, indent: usize) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", php_single_quote_escape(s)),
        Value::Array(items) => {
            let pad = " ".repeat(indent + 2);
            let mut out = String::from("array (\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&export_element(&pad, &i.to_string(), item, indent + 2));
            }
            out.push_str(&" ".repeat(indent));
            out.push(')');
            out
        }
        Value::Object(map) => {
            let pad = " ".repeat(indent + 2);
            let mut out = String::from("array (\n");
            for (k, v) in map {
                let key = format!("'{}'", php_single_quote_escape(k));
                out.push_str(&export_element(&pad, &key, v, indent + 2));
            }
            out.push_str(&" ".repeat(indent));
            out.push(')');
            out
        }
    }
}

/// One `key => value,` line of a `var_export`'d array. PHP puts a
/// scalar value on the same line, but for a nested array it emits
/// `key => ` (with a trailing space) then the `array (` on the next line
/// at the same indent.
fn export_element(pad: &str, key: &str, value: &Value, indent: usize) -> String {
    match value {
        Value::Array(_) | Value::Object(_) => {
            format!("{pad}{key} => \n{pad}{},\n", var_export(value, indent))
        }
        _ => format!("{pad}{key} => {},\n", var_export(value, indent)),
    }
}

/// Escape a string for a PHP single-quoted literal: `\` → `\\`, `'` →
/// `\'` (backslash first). Matches what `var_export` emits.
fn php_single_quote_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn installed(packages: Value) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("packages".into(), packages);
        Value::Object(m)
    }

    #[test]
    fn renders_empty_manifest_like_var_export() {
        let m = build_package_manifest(&installed(json!([])), &json!({}));
        assert_eq!(render_packages_php(&m), "<?php return array (\n);");
    }

    #[test]
    fn renders_realistic_manifest_byte_for_byte() {
        // Expected captured from PHP 8.4 `var_export`.
        let inst = installed(json!([
            {"name": "spatie/laravel-permission", "extra": {"laravel": {"providers": ["Spatie\\Permission\\PermissionServiceProvider"]}}},
            {"name": "acme/pkg", "extra": {"laravel": {"providers": ["Acme\\A", "Acme\\B"], "aliases": {"Foo": "Acme\\Foo"}}}}
        ]));
        let m = build_package_manifest(&inst, &json!({}));
        let expected = "<?php return array (\n  \
'spatie/laravel-permission' => \n  array (\n    \
'providers' => \n    array (\n      \
0 => 'Spatie\\\\Permission\\\\PermissionServiceProvider',\n    ),\n  ),\n  \
'acme/pkg' => \n  array (\n    \
'providers' => \n    array (\n      \
0 => 'Acme\\\\A',\n      1 => 'Acme\\\\B',\n    ),\n    \
'aliases' => \n    array (\n      \
'Foo' => 'Acme\\\\Foo',\n    ),\n  ),\n);";
        assert_eq!(render_packages_php(&m), expected);
    }

    #[test]
    fn drops_packages_without_laravel_config() {
        let inst = installed(json!([
            {"name": "acme/plain"},
            {"name": "acme/empty", "extra": {"laravel": {}}},
            {"name": "acme/real", "extra": {"laravel": {"providers": ["Acme\\P"]}}}
        ]));
        let m = build_package_manifest(&inst, &json!({}));
        let obj = m.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert!(obj.contains_key("acme/real"));
    }

    #[test]
    fn root_dont_discover_excludes_package() {
        let inst = installed(json!([
            {"name": "acme/a", "extra": {"laravel": {"providers": ["A"]}}},
            {"name": "acme/b", "extra": {"laravel": {"providers": ["B"]}}}
        ]));
        let root = json!({"laravel": {"dont-discover": ["acme/a"]}});
        let m = build_package_manifest(&inst, &root);
        let obj = m.as_object().unwrap();
        assert!(!obj.contains_key("acme/a"));
        assert!(obj.contains_key("acme/b"));
    }

    #[test]
    fn root_dont_discover_star_excludes_all() {
        let inst = installed(json!([
            {"name": "acme/a", "extra": {"laravel": {"providers": ["A"]}}}
        ]));
        let m = build_package_manifest(&inst, &json!({"laravel": {"dont-discover": ["*"]}}));
        assert!(m.as_object().unwrap().is_empty());
    }

    #[test]
    fn package_declared_dont_discover_excludes_other_package() {
        // A package later in the list can ignore an earlier one.
        let inst = installed(json!([
            {"name": "acme/victim", "extra": {"laravel": {"providers": ["V"]}}},
            {"name": "acme/bully", "extra": {"laravel": {"providers": ["B"], "dont-discover": ["acme/victim"]}}}
        ]));
        let m = build_package_manifest(&inst, &json!({}));
        let obj = m.as_object().unwrap();
        assert!(!obj.contains_key("acme/victim"));
        assert!(obj.contains_key("acme/bully"));
    }

    #[test]
    fn recognizes_canonical_post_autoload_dump() {
        let scripts = json!({
            "post-autoload-dump": [
                "Illuminate\\Foundation\\ComposerScripts::postAutoloadDump",
                "@php artisan package:discover --ansi"
            ]
        });
        assert!(blocking_post_autoload_dump(&scripts).is_empty());
        // Without flags + single-string form also recognized.
        assert!(blocking_post_autoload_dump(
            &json!({"post-autoload-dump": "@php artisan package:discover"})
        ).is_empty());
        // Missing scripts / section → nothing to reproduce.
        assert!(blocking_post_autoload_dump(&json!({})).is_empty());
        assert!(blocking_post_autoload_dump(&Value::Null).is_empty());
    }

    #[test]
    fn trailing_extra_steps_are_allowed() {
        // Custom steps AFTER the defaults don't block reproduction — the
        // defaults still run first.
        let scripts = json!({
            "post-autoload-dump": [
                "Illuminate\\Foundation\\ComposerScripts::postAutoloadDump",
                "@php artisan package:discover --ansi",
                "@php artisan vendor:publish --tag=laravel-assets",
                "@php artisan ziggy:generate"
            ]
        });
        assert!(blocking_post_autoload_dump(&scripts).is_empty());
    }

    #[test]
    fn steps_before_or_between_defaults_block() {
        // Before the defaults.
        let before = json!({
            "post-autoload-dump": [
                "@php artisan something:custom",
                "Illuminate\\Foundation\\ComposerScripts::postAutoloadDump",
                "@php artisan package:discover --ansi"
            ]
        });
        assert_eq!(
            blocking_post_autoload_dump(&before),
            vec!["@php artisan something:custom".to_string()]
        );

        // Between the two defaults.
        let between = json!({
            "post-autoload-dump": [
                "Illuminate\\Foundation\\ComposerScripts::postAutoloadDump",
                "@php artisan something:custom",
                "@php artisan package:discover --ansi"
            ]
        });
        assert_eq!(
            blocking_post_autoload_dump(&between),
            vec!["@php artisan something:custom".to_string()]
        );
    }

    #[test]
    fn no_recognizable_default_blocks_everything() {
        // Discovery replaced/renamed → can't reproduce; block on all of it.
        let scripts = json!({"post-autoload-dump": ["@php artisan package:discover-v2"]});
        assert_eq!(
            blocking_post_autoload_dump(&scripts),
            vec!["@php artisan package:discover-v2".to_string()]
        );
    }

    #[test]
    fn escapes_quotes_in_values() {
        let inst = installed(json!([
            {"name": "acme/q", "extra": {"laravel": {"providers": ["A'B"]}}}
        ]));
        let m = build_package_manifest(&inst, &json!({}));
        assert!(render_packages_php(&m).contains("'A\\'B'"));
    }
}
