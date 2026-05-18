//! Emit `vendor/composer/installed.json` and
//! `vendor/composer/installed.php`.
//!
//! Composer's `FilesystemRepository::write` regenerates both files on
//! every `composer install` / `dump-autoload`:
//!
//! - `installed.json` is a re-serialization of `composer.lock`'s
//!   packages, reshaped through `ArrayDumper::dump`, with
//!   `version_normalized`, `installation-source`, and `install-path`
//!   spliced in. Wrapped as `{"packages":[...], "dev":bool,
//!   "dev-package-names":[...]}`. Pretty-printed with Composer's
//!   `JsonFormatter` (4-space indent, `: ` after keys, empty
//!   `{}`/`[]` stay inline, slashes unescaped).
//!
//! - `installed.php` is consumed by the vendored
//!   `Composer\InstalledVersions` class at runtime (`getVersion()`,
//!   `isInstalled()`, etc.). It has `'root'` and `'versions'` keys;
//!   `versions` contains every package *and* the root. Format is
//!   `var_export`-style array with `install_path` rewritten to
//!   `__DIR__ . '/...'`.
//!
//! Field-ordering inside each package entry mirrors
//! `Composer\Package\Dumper\ArrayDumper::dump`; the canonical key
//! sequence is reproduced verbatim in `package_to_installed_entry`.
//!
//! Version normalization is the minimal subset that covers our
//! fixtures: pad `X.Y.Z` to `X.Y.Z.0` and strip a leading `v` or a
//! `+build` suffix. Full `VersionParser` semantics (dev-branches,
//! stability suffixes) land when a fixture requires them.

use std::collections::HashSet;
use std::fmt::Write;
use std::path::Path;

use serde_json::{Map, Value};

use crate::DumpError;

/// Re-parse `composer.lock` as raw JSON. `lock::read_lock` already
/// gives us a typed view tuned for the autoloader pass; `installed.json`
/// needs the full per-package field set so we read the file again here
/// rather than thread every optional field through `lock::Package`.
fn read_lock_value(project_root: &Path) -> Result<Value, DumpError> {
    let path = project_root.join("composer.lock");
    let bytes = std::fs::read(&path)?;
    serde_json::from_slice(&bytes).map_err(|e| DumpError::Lock(format!("{path:?}: {e}")))
}

fn read_manifest_value(project_root: &Path) -> Result<Value, DumpError> {
    let path = project_root.join("composer.json");
    let bytes = std::fs::read(&path)?;
    serde_json::from_slice(&bytes).map_err(|e| DumpError::Manifest(format!("{path:?}: {e}")))
}

pub(crate) fn emit_installed_json(
    project_root: &Path,
    no_dev: bool,
) -> Result<String, DumpError> {
    let lock = read_lock_value(project_root)?;
    let lock_obj = lock
        .as_object()
        .ok_or_else(|| DumpError::Lock("expected top-level object".into()))?;

    let empty: Vec<Value> = Vec::new();
    let prod = lock_obj
        .get("packages")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let dev = if no_dev {
        &empty[..]
    } else {
        lock_obj
            .get("packages-dev")
            .and_then(|v| v.as_array())
            .map(|a| &a[..])
            .unwrap_or(&[])
    };

    let mut dev_names: Vec<String> = dev
        .iter()
        .filter_map(|p| p.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect();
    dev_names.sort();

    // Reshape and sort packages alphabetically. Composer's
    // FilesystemRepository::write does `usort(..., strcmp($a['name'],
    // $b['name']))` after collecting both sets.
    let mut packages: Vec<Map<String, Value>> = prod
        .iter()
        .chain(dev.iter())
        .filter_map(|p| p.as_object())
        .map(package_to_installed_entry)
        .collect();
    packages.sort_by(|a, b| {
        let an = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let bn = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        an.cmp(bn)
    });

    let mut root = Map::new();
    root.insert(
        "packages".into(),
        Value::Array(packages.into_iter().map(Value::Object).collect()),
    );
    root.insert("dev".into(), Value::Bool(!no_dev));
    root.insert(
        "dev-package-names".into(),
        Value::Array(dev_names.into_iter().map(Value::String).collect()),
    );

    Ok(format_composer_json(&Value::Object(root)))
}

/// Reshape one `composer.lock` package entry into its
/// `installed.json` form. Field order mirrors
/// `Composer\Package\Dumper\ArrayDumper::dump`: name, version,
/// version_normalized, target-dir, source, dist, link types (require,
/// conflict, provide, replace, require-dev), suggest, time, bin, type,
/// extra, installation-source, autoload, autoload-dev,
/// notification-url, include-path, php-ext, archive, scripts, license,
/// authors, description, homepage, keywords, repositories, support,
/// funding, transport-options, install-path.
fn package_to_installed_entry(pkg: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    let copy = |out: &mut Map<String, Value>, k: &str| {
        if let Some(v) = pkg.get(k) {
            out.insert(k.into(), v.clone());
        }
    };

    copy(&mut out, "name");
    let name = pkg.get("name").and_then(|v| v.as_str()).unwrap_or("");

    if let Some(v) = pkg.get("version") {
        out.insert("version".into(), v.clone());
        let nv = normalize_version(v.as_str().unwrap_or(""));
        out.insert("version_normalized".into(), Value::String(nv));
    }

    copy(&mut out, "target-dir");
    copy(&mut out, "source");
    copy(&mut out, "dist");

    for k in ["require", "conflict", "provide", "replace", "require-dev"] {
        copy(&mut out, k);
    }

    copy(&mut out, "suggest");
    copy(&mut out, "time");

    copy(&mut out, "bin");
    copy(&mut out, "type");
    copy(&mut out, "extra");

    // Composer's `BasePackage::getInstallationSource` reflects what
    // the installer actually used. For path repos (and packagist
    // downloads under default settings) this is "dist"; "source" only
    // appears under `--prefer-source` or when a package has no dist.
    let installation_source = if pkg.contains_key("dist") {
        "dist"
    } else if pkg.contains_key("source") {
        "source"
    } else {
        "dist"
    };
    out.insert(
        "installation-source".into(),
        Value::String(installation_source.into()),
    );

    copy(&mut out, "autoload");
    copy(&mut out, "autoload-dev");
    copy(&mut out, "notification-url");
    copy(&mut out, "include-path");
    copy(&mut out, "php-ext");

    copy(&mut out, "archive");
    copy(&mut out, "scripts");
    copy(&mut out, "license");
    copy(&mut out, "authors");
    copy(&mut out, "description");
    copy(&mut out, "homepage");
    copy(&mut out, "keywords");
    copy(&mut out, "repositories");
    copy(&mut out, "support");
    copy(&mut out, "funding");

    copy(&mut out, "transport-options");

    // install-path: from vendor/composer/ to vendor/<name>/ is
    // `../<name>`. `findShortestPath(repoDir, packagePath, true)`
    // produces this without a trailing slash for any non-empty
    // sub-path.
    out.insert("install-path".into(), Value::String(format!("../{name}")));

    out
}

pub(crate) fn emit_installed_php(
    project_root: &Path,
    no_dev: bool,
) -> Result<String, DumpError> {
    let lock = read_lock_value(project_root)?;
    let manifest = read_manifest_value(project_root)?;

    let lock_obj = lock
        .as_object()
        .ok_or_else(|| DumpError::Lock("expected top-level object".into()))?;

    let empty: Vec<Value> = Vec::new();
    let prod = lock_obj
        .get("packages")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let dev = if no_dev {
        &empty[..]
    } else {
        lock_obj
            .get("packages-dev")
            .and_then(|v| v.as_array())
            .map(|a| &a[..])
            .unwrap_or(&[])
    };

    let dev_names: HashSet<String> = dev
        .iter()
        .filter_map(|p| p.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect();

    let mut packages: Vec<PkgEntry> = prod
        .iter()
        .chain(dev.iter())
        .filter_map(|p| p.as_object())
        .map(|p| pkg_entry_from_lock(p, &dev_names))
        .collect();
    packages.sort_by(|a, b| a.name.cmp(&b.name));

    let manifest_obj = manifest
        .as_object()
        .ok_or_else(|| DumpError::Manifest("expected top-level object".into()))?;
    let root = root_entry_from_manifest(manifest_obj);

    let dev_mode = !no_dev;
    Ok(format_installed_php(&root, &packages, dev_mode))
}

#[derive(Clone)]
struct PkgEntry {
    name: String,
    pretty_version: String,
    version: String,
    reference: Option<String>,
    r#type: String,
    install_path: String,
    dev_requirement: bool,
}

struct RootEntry {
    name: String,
    pretty_version: String,
    version: String,
    reference: Option<String>,
    r#type: String,
    install_path: String,
}

fn pkg_entry_from_lock(pkg: &Map<String, Value>, dev_names: &HashSet<String>) -> PkgEntry {
    let name = pkg
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let pretty_version = pkg
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let version = normalize_version(&pretty_version);
    // Mirrors `FilesystemRepository::dumpInstalledPackage`:
    //   $reference = $installationSource === 'source'
    //              ? $sourceReference : $distReference;
    //   if ($reference === null) $reference = $sourceReference
    //                                       ?: $distReference ?: null;
    // We always pick installation-source == 'dist' for our fixtures.
    let reference = pkg
        .get("dist")
        .and_then(|d| d.get("reference"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            pkg.get("source")
                .and_then(|d| d.get("reference"))
                .and_then(|v| v.as_str())
                .map(String::from)
        });
    let ty = pkg
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("library")
        .to_string();
    let install_path = format!("../{name}");
    let dev_requirement = dev_names.contains(&name);
    PkgEntry {
        name,
        pretty_version,
        version,
        reference,
        r#type: ty,
        install_path,
        dev_requirement,
    }
}

fn root_entry_from_manifest(manifest: &Map<String, Value>) -> RootEntry {
    let name = manifest
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("__root__")
        .to_string();
    let ty = manifest
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("library")
        .to_string();
    let (pretty_version, version) = match manifest.get("version").and_then(|v| v.as_str()) {
        Some(v) => (v.to_string(), normalize_version(v)),
        // Composer's RootPackageLoader uses VersionGuesser, which
        // ultimately falls back to "1.0.0+no-version-set" when no VCS
        // tag is available either. Path-repo fixtures never have one.
        None => ("1.0.0+no-version-set".to_string(), "1.0.0.0".to_string()),
    };
    RootEntry {
        name,
        pretty_version,
        version,
        reference: None,
        r#type: ty,
        // findShortestPath(vendor/composer/, project_root/, true)
        // is `../../`; the trailing slash is what Composer adds when
        // the relative path lands on the source's ancestor.
        install_path: "../../".to_string(),
    }
}

fn format_installed_php(root: &RootEntry, packages: &[PkgEntry], dev_mode: bool) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str("<?php return array(\n");

    // root block
    out.push_str("    'root' => array(\n");
    write_kv(&mut out, 8, "name", &php_str(&root.name));
    write_kv(&mut out, 8, "pretty_version", &php_str(&root.pretty_version));
    write_kv(&mut out, 8, "version", &php_str(&root.version));
    write_kv(&mut out, 8, "reference", &php_maybe_null(root.reference.as_deref()));
    write_kv(&mut out, 8, "type", &php_str(&root.r#type));
    write_kv(
        &mut out,
        8,
        "install_path",
        &format!("__DIR__ . {}", php_str(&format!("/{}", root.install_path))),
    );
    write_kv(&mut out, 8, "aliases", "array()");
    write_kv(&mut out, 8, "dev", if dev_mode { "true" } else { "false" });
    out.push_str("    ),\n");

    // versions block: every package + the root, alphabetical.
    out.push_str("    'versions' => array(\n");
    let mut all: Vec<PkgEntry> = packages.to_vec();
    all.push(PkgEntry {
        name: root.name.clone(),
        pretty_version: root.pretty_version.clone(),
        version: root.version.clone(),
        reference: root.reference.clone(),
        r#type: root.r#type.clone(),
        install_path: root.install_path.clone(),
        dev_requirement: false,
    });
    all.sort_by(|a, b| a.name.cmp(&b.name));

    for pkg in &all {
        let _ = writeln!(out, "        {} => array(", php_str(&pkg.name));
        write_kv(&mut out, 12, "pretty_version", &php_str(&pkg.pretty_version));
        write_kv(&mut out, 12, "version", &php_str(&pkg.version));
        write_kv(
            &mut out,
            12,
            "reference",
            &php_maybe_null(pkg.reference.as_deref()),
        );
        write_kv(&mut out, 12, "type", &php_str(&pkg.r#type));
        write_kv(
            &mut out,
            12,
            "install_path",
            &format!("__DIR__ . {}", php_str(&format!("/{}", pkg.install_path))),
        );
        write_kv(&mut out, 12, "aliases", "array()");
        write_kv(
            &mut out,
            12,
            "dev_requirement",
            if pkg.dev_requirement { "true" } else { "false" },
        );
        out.push_str("        ),\n");
    }

    out.push_str("    ),\n");
    out.push_str(");\n");
    out
}

fn write_kv(out: &mut String, indent: usize, key: &str, value: &str) {
    for _ in 0..indent {
        out.push(' ');
    }
    let _ = writeln!(out, "'{key}' => {value},");
}

fn php_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn php_maybe_null(s: Option<&str>) -> String {
    match s {
        Some(v) => php_str(v),
        None => "null".to_string(),
    }
}

/// Minimal `VersionParser::normalize` port — enough for the path-repo
/// fixtures we ship today. Handles:
/// - leading `v` strip
/// - `+build` metadata strip
/// - `X.Y.Z` padded to `X.Y.Z.0`; already-4-part input unchanged
/// - `-suffix` preserved verbatim (no Composer-style canonicalization
///   of alpha/beta/RC casing — no fixture exercises it yet)
fn normalize_version(s: &str) -> String {
    let s = s.strip_prefix('v').unwrap_or(s);
    let s = match s.find('+') {
        Some(idx) => &s[..idx],
        None => s,
    };
    let (numeric, suffix) = match s.find('-') {
        Some(idx) => (&s[..idx], Some(&s[idx..])),
        None => (s, None),
    };
    let mut parts: Vec<String> = numeric.split('.').map(String::from).collect();
    while parts.len() < 4 {
        parts.push("0".into());
    }
    parts.truncate(4);
    let normalized = parts.join(".");
    match suffix {
        Some(sfx) => format!("{normalized}{sfx}"),
        None => normalized,
    }
}

/// Composer-style JSON pretty-printer. Indent is four spaces, `:` is
/// followed by one space, empty `{}` and `[]` stay on the line they
/// open on. Slashes are emitted unescaped (Composer's `JsonFormatter`
/// passes `unescapeSlashes = true` for `installed.json`). Trailing
/// newline.
fn format_composer_json(v: &Value) -> String {
    let mut out = String::with_capacity(2048);
    emit_json(v, &mut out, 0);
    out.push('\n');
    out
}

fn emit_json(v: &Value, out: &mut String, level: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => emit_json_string(s, out),
        Value::Array(arr) => {
            if arr.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                indent_json(out, level + 1);
                emit_json(item, out, level + 1);
            }
            out.push('\n');
            indent_json(out, level);
            out.push(']');
        }
        Value::Object(obj) => {
            if obj.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, val)) in obj.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                indent_json(out, level + 1);
                emit_json_string(k, out);
                out.push_str(": ");
                emit_json(val, out, level + 1);
            }
            out.push('\n');
            indent_json(out, level);
            out.push('}');
        }
    }
}

fn indent_json(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("    ");
    }
}

fn emit_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_basic() {
        assert_eq!(normalize_version("1.0.0"), "1.0.0.0");
        assert_eq!(normalize_version("v2.5.0"), "2.5.0.0");
        assert_eq!(normalize_version("1.0.0+no-version-set"), "1.0.0.0");
        assert_eq!(normalize_version("3.1.2.4"), "3.1.2.4");
    }

    #[test]
    fn empty_array_inline() {
        let v: Value = serde_json::json!({ "dev-package-names": [] });
        let s = format_composer_json(&v);
        assert!(s.contains("\"dev-package-names\": []"));
        assert!(!s.contains("[\n"));
    }

    #[test]
    fn php_escapes_quotes_and_backslashes() {
        assert_eq!(php_str("a'b"), "'a\\'b'");
        assert_eq!(php_str("a\\b"), "'a\\\\b'");
    }
}
