//! Infer a PHP version constraint when neither `composer.json`'s
//! `require.php` nor `bougie.toml`'s `[php]version` is set.
//!
//! Two signals, tried in order:
//!
//! 1. **Recipe-based default.** If `composer.json`'s `require` names a
//!    known framework root (today: Magento), map its declared version
//!    to the framework's officially recommended PHP range.
//! 2. **Lockfile-based intersection.** If `composer.lock` exists, AND
//!    together `platform.php` and every locked package's `require.php`.
//!    Offline, no network.
//!
//! Both signals return `(constraint, source)` where `source` is a short
//! human-readable label used in the user-facing notice. When neither
//! fires, returns `None` and the caller falls back to today's
//! behavior (hard error for `bougie sync`, `>=8.0` for `bougie run`).

use composer_semver::Constraint;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Try to infer a PHP constraint from on-disk project files. Returns
/// `(constraint, source_label)` or `None`.
pub fn infer(project_root: &Path) -> Option<(Constraint, String)> {
    infer_raw(project_root).map(|i| (i.constraint, i.source))
}

/// An inferred PHP constraint plus, when one exists, its written
/// string form. The recipe lane reads a matrix row (`"~8.3.0 ||
/// ~8.4.0"`) so it has a raw form; a `composer.lock` intersection is
/// synthesized from many requires and has none.
#[derive(Debug, Clone)]
pub struct InferredPhp {
    pub constraint: Constraint,
    /// Written form of `constraint`, for callers that need to hand it
    /// to string-typed APIs (the PHP auto-installer takes specs as
    /// strings — `bgx`'s project context uses this for its installer
    /// fallback).
    pub raw: Option<String>,
    /// Human-readable label for the notice ("magento/... 2.4.7",
    /// "composer.lock").
    pub source: String,
}

/// [`infer`], keeping the raw constraint string when the signal has
/// one.
pub fn infer_raw(project_root: &Path) -> Option<InferredPhp> {
    if let Ok(text) = fs::read_to_string(project_root.join("composer.json"))
        && let Some((constraint, raw, source)) = magento_default(&text)
    {
        return Some(InferredPhp {
            constraint,
            raw: Some(raw.to_string()),
            source,
        });
    }
    if let Ok(text) = fs::read_to_string(project_root.join("composer.lock"))
        && let Some((constraint, source)) = lockfile_intersection(&text)
    {
        return Some(InferredPhp {
            constraint,
            raw: None,
            source,
        });
    }
    None
}

/// Map a Magento or Mage-OS project to the officially recommended PHP
/// range.
///
/// We read the constraint on one of the four known root package names
/// (both vendors × both aliases), pull out its first written version,
/// and look up the matrix row. Magento constraints are always pinned
/// tight (`^2.4.7`, `~2.4.6`, `2.4.5-p1`), so the first version
/// mention is the deciding lower bound for the matrix.
fn magento_default(composer_json: &str) -> Option<(Constraint, &'static str, String)> {
    let v: Value = serde_json::from_str(composer_json).ok()?;
    let require = v.get("require").and_then(Value::as_object)?;

    // Try upstream Magento names first.
    if let Some((pkg, raw)) =
        ["magento/product-community-edition", "magento/magento2-base"]
            .iter()
            .find_map(|k| require.get(*k).and_then(Value::as_str).map(|s| (*k, s)))
    {
        let (major, minor, patch) = extract_first_version(raw)?;
        let php = magento_php_for(major, minor, patch)?;
        let c = Constraint::parse(php).ok()?;
        let pv = match patch {
            Some(p) => format!("{major}.{minor}.{p}"),
            None => format!("{major}.{minor}"),
        };
        return Some((c, php, format!("{pkg} {pv}")));
    }

    // Fall through to Mage-OS fork names.
    if let Some((pkg, raw)) =
        ["mage-os/product-community-edition", "mage-os/magento2-base"]
            .iter()
            .find_map(|k| require.get(*k).and_then(Value::as_str).map(|s| (*k, s)))
    {
        let (major, minor, _patch) = extract_first_version(raw)?;
        let php = mageos_php_for(major, minor)?;
        let c = Constraint::parse(php).ok()?;
        let pv = match extract_first_version(raw) {
            Some((maj, min, Some(p))) => format!("{maj}.{min}.{p}"),
            Some((maj, min, None)) => format!("{maj}.{min}"),
            None => format!("{major}.{minor}"),
        };
        return Some((c, php, format!("{pkg} {pv}")));
    }

    None
}

/// PHP range Adobe officially supports for a given Magento version, per
/// the "System Requirements" matrix. Conservative when in doubt — for
/// 2.4 minors past the latest documented row we pick the newest row.
fn magento_php_for(major: u64, minor: u64, patch: Option<u64>) -> Option<&'static str> {
    if major != 2 {
        return None;
    }
    if minor != 4 {
        return None;
    }
    match patch.unwrap_or(0) {
        0..=3 => Some("~7.4.0"),
        4 | 5 => Some("~8.1.0"),
        6 => Some("~8.1.0 || ~8.2.0 || ~8.3.0"),
        7 => Some("~8.2.0 || ~8.3.0"),
        _ => Some("~8.3.0 || ~8.4.0"),
    }
}

/// PHP range Mage-OS recommends for a given fork version.
///
/// Mage-OS uses its own 1.x / 2.x / 3.x versioning (independent of
/// Magento's 2.4.x train). When the major is unknown/future we pick
/// the widest recent row so the resolver has room to maneuver.
fn mageos_php_for(major: u64, minor: u64) -> Option<&'static str> {
    let _ = minor; // minor not used yet; kept for future sub-rows
    match major {
        1 => Some("~8.1.0 || ~8.2.0 || ~8.3.0"),
        2 => Some("~8.2.0 || ~8.3.0 || ~8.4.0"),
        _ => Some("~8.3.0 || ~8.4.0 || ~8.5.0"),
    }
}

/// Pull the first `major.minor[.patch]` out of an arbitrary constraint
/// string. We skip leading non-digits (`^`, `~`, `>=`, etc.) and then
/// read up to three dot-separated integer segments.
fn extract_first_version(s: &str) -> Option<(u64, u64, Option<u64>)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let mut nums = [0u64; 3];
    let mut count = 0;
    while count < 3 && i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            break;
        }
        nums[count] = s[start..i].parse().ok()?;
        count += 1;
        if i >= bytes.len() || bytes[i] != b'.' {
            break;
        }
        i += 1;
    }
    if count < 2 {
        return None;
    }
    Some((nums[0], nums[1], if count >= 3 { Some(nums[2]) } else { None }))
}

/// AND together `platform.php` and every locked package's `require.php`
/// from `composer.lock`. We parse each constraint string independently
/// (so internal `||` groups keep their proper precedence) and combine
/// the results into a single `Constraint::And`.
fn lockfile_intersection(composer_lock: &str) -> Option<(Constraint, String)> {
    let v: Value = serde_json::from_str(composer_lock).ok()?;
    let mut raw: Vec<String> = Vec::new();
    if let Some(p) = v
        .get("platform")
        .and_then(Value::as_object)
        .and_then(|p| p.get("php"))
        .and_then(Value::as_str)
    {
        raw.push(p.into());
    }
    for key in ["packages", "packages-dev"] {
        if let Some(arr) = v.get(key).and_then(Value::as_array) {
            for pkg in arr {
                if let Some(req) = pkg
                    .get("require")
                    .and_then(Value::as_object)
                    .and_then(|r| r.get("php"))
                    .and_then(Value::as_str)
                {
                    raw.push(req.into());
                }
            }
        }
    }
    let parsed: Vec<Constraint> = raw
        .iter()
        .filter_map(|s| Constraint::parse(s).ok())
        .collect();
    let combined = match parsed.len() {
        0 => return None,
        1 => parsed.into_iter().next().unwrap(),
        _ => Constraint::And(parsed),
    };
    Some((combined, "composer.lock".into()))
}

/// Adobe's required PHP extensions for Magento 2.4 ("System
/// Requirements"). `opcache` is officially "recommended" but every
/// production install ships with it on, so we treat it as required.
const MAGENTO_EXTENSIONS: &[&str] = &[
    "bcmath",
    "ctype",
    "curl",
    "dom",
    "fileinfo",
    "gd",
    "hash",
    "iconv",
    "intl",
    "libxml",
    "mbstring",
    "opcache",
    "openssl",
    "pcre",
    "pdo_mysql",
    "simplexml",
    "soap",
    "sockets",
    "sodium",
    "tokenizer",
    "xmlwriter",
    "xsl",
    "zip",
];

/// Try to infer a set of required PHP extensions from on-disk project
/// files. Returns the union of:
///
/// - **Recipe.** When the project is a recognised framework root
///   (today: Magento), include that framework's required extensions.
/// - **Lockfile.** When `composer.lock` exists, include every `ext-*`
///   listed in any locked package's `require`.
///
/// Returns `(names, sources)` — `sources` lists the human-readable
/// labels for the notice (e.g. `["magento/product-community-edition
/// 2.4.7", "composer.lock"]`). The caller emits a single notice with
/// both. Empty set means no signal fired.
pub fn infer_extensions(project_root: &Path) -> (BTreeSet<String>, Vec<String>) {
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut sources: Vec<String> = Vec::new();

    if let Ok(text) = fs::read_to_string(project_root.join("composer.json"))
        && let Some((exts, src)) = magento_extensions(&text)
    {
        names.extend(exts.iter().map(|s| (*s).to_string()));
        sources.push(src);
    }
    if let Ok(text) = fs::read_to_string(project_root.join("composer.lock")) {
        let from_lock = lockfile_extensions(&text);
        if !from_lock.is_empty() {
            names.extend(from_lock);
            sources.push("composer.lock".into());
        }
    }
    (names, sources)
}

/// Magento / Mage-OS required extension set, gated on the same
/// package-name detection used by [`magento_default`]. Both vendors
/// require the same extension set (Mage-OS is a fork with the same
/// system requirements).
fn magento_extensions(composer_json: &str) -> Option<(&'static [&'static str], String)> {
    let v: Value = serde_json::from_str(composer_json).ok()?;
    let require = v.get("require").and_then(Value::as_object)?;
    let all_names = [
        "magento/product-community-edition",
        "magento/magento2-base",
        "mage-os/product-community-edition",
        "mage-os/magento2-base",
    ];
    let (pkg, raw) = all_names
        .iter()
        .find_map(|k| require.get(*k).and_then(Value::as_str).map(|s| (*k, s)))?;
    let label = match extract_first_version(raw) {
        Some((maj, min, Some(p))) => format!("{pkg} {maj}.{min}.{p}"),
        Some((maj, min, None)) => format!("{pkg} {maj}.{min}"),
        None => pkg.to_string(),
    };
    Some((MAGENTO_EXTENSIONS, label))
}

/// Walk `packages` + `packages-dev` and collect every `require` key
/// starting with `ext-`, stripped of the prefix.
fn lockfile_extensions(composer_lock: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Ok(v) = serde_json::from_str::<Value>(composer_lock) else {
        return out;
    };
    for key in ["packages", "packages-dev"] {
        let Some(arr) = v.get(key).and_then(Value::as_array) else {
            continue;
        };
        for pkg in arr {
            let Some(req) = pkg.get("require").and_then(Value::as_object) else {
                continue;
            };
            for k in req.keys() {
                if let Some(name) = k.strip_prefix("ext-") {
                    out.insert(name.to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use composer_semver::Version;

    fn parses(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn magento_247_picks_82_or_83() {
        let composer = r#"{"require":{"magento/product-community-edition":"2.4.7-p3"}}"#;
        let (c, _raw, src) = magento_default(composer).unwrap();
        assert!(src.contains("2.4.7"));
        assert!(c.matches(&parses("8.2.20")));
        assert!(c.matches(&parses("8.3.10")));
        assert!(!c.matches(&parses("8.1.20")));
        assert!(!c.matches(&parses("8.4.0")));
    }

    #[test]
    fn magento_246_caret_range() {
        let composer = r#"{"require":{"magento/product-community-edition":"^2.4.6"}}"#;
        let (c, _raw, _) = magento_default(composer).unwrap();
        assert!(c.matches(&parses("8.1.20")));
        assert!(c.matches(&parses("8.2.20")));
        assert!(c.matches(&parses("8.3.10")));
        assert!(!c.matches(&parses("8.0.30")));
        assert!(!c.matches(&parses("8.4.0")));
    }

    #[test]
    fn magento_base_alias_works() {
        let composer = r#"{"require":{"magento/magento2-base":"~2.4.5"}}"#;
        let (c, _raw, src) = magento_default(composer).unwrap();
        assert!(src.contains("magento2-base"));
        assert!(c.matches(&parses("8.1.20")));
    }

    #[test]
    fn non_magento_yields_none() {
        let composer = r#"{"require":{"laravel/framework":"^11.0"}}"#;
        assert!(magento_default(composer).is_none());
    }

    #[test]
    fn extract_first_version_handles_prefixes() {
        assert_eq!(extract_first_version("^2.4.7"), Some((2, 4, Some(7))));
        assert_eq!(extract_first_version("~2.4.6"), Some((2, 4, Some(6))));
        assert_eq!(extract_first_version(">=2.4 <2.5"), Some((2, 4, None)));
        assert_eq!(extract_first_version("2.4.7-p3"), Some((2, 4, Some(7))));
        assert_eq!(extract_first_version("garbage"), None);
    }

    #[test]
    fn lockfile_intersects_platform_and_packages() {
        let lock = r#"{
            "platform": {"php": ">=8.1"},
            "packages": [
                {"name": "a/b", "require": {"php": "^7.4 || ^8.0"}},
                {"name": "c/d", "require": {"php": ">=8.2"}}
            ],
            "packages-dev": [
                {"name": "e/f", "require": {"php": "<8.4"}}
            ]
        }"#;
        let (c, src) = lockfile_intersection(lock).unwrap();
        assert_eq!(src, "composer.lock");
        assert!(c.matches(&parses("8.2.10")));
        assert!(c.matches(&parses("8.3.5")));
        assert!(!c.matches(&parses("8.1.20"))); // <8.2
        assert!(!c.matches(&parses("8.4.0"))); // >=8.4 excluded
        assert!(!c.matches(&parses("7.4.30"))); // platform >=8.1 excluded
    }

    #[test]
    fn lockfile_without_php_constraints_returns_none() {
        let lock = r#"{"packages": [{"name": "a/b", "require": {"ext-foo": "*"}}]}"#;
        assert!(lockfile_intersection(lock).is_none());
    }

    #[test]
    fn magento_extensions_returns_recommended_set() {
        let composer = r#"{"require":{"magento/product-community-edition":"2.4.7-p3"}}"#;
        let (exts, src) = magento_extensions(composer).unwrap();
        assert!(src.contains("2.4.7"));
        assert!(exts.contains(&"pdo_mysql"));
        assert!(exts.contains(&"intl"));
        assert!(exts.contains(&"opcache"));
    }

    #[test]
    fn lockfile_extensions_unions_packages() {
        let lock = r#"{
            "packages": [
                {"name": "a/b", "require": {"ext-redis": "*", "ext-intl": "*"}},
                {"name": "c/d", "require": {"ext-curl": "*", "php": ">=8.1"}}
            ],
            "packages-dev": [
                {"name": "e/f", "require": {"ext-xdebug": "*"}}
            ]
        }"#;
        let set = lockfile_extensions(lock);
        assert!(set.contains("redis"));
        assert!(set.contains("intl"));
        assert!(set.contains("curl"));
        assert!(set.contains("xdebug"));
        assert!(!set.contains("php"));
    }

    #[test]
    fn infer_extensions_unions_recipe_and_lockfile() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(
            dir.path().join("composer.json"),
            r#"{"require":{"magento/product-community-edition":"2.4.7-p3"}}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("composer.lock"),
            r#"{"packages":[{"name":"a/b","require":{"ext-redis":"*"}}]}"#,
        )
        .unwrap();
        let (names, sources) = infer_extensions(dir.path());
        assert!(names.contains("pdo_mysql"));
        assert!(names.contains("redis"));
        assert_eq!(sources.len(), 2);
    }

    #[test]
    fn infer_extensions_no_signals_yields_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let (names, sources) = infer_extensions(dir.path());
        assert!(names.is_empty());
        assert!(sources.is_empty());
    }

    // ── Mage-OS tests ────────────────────────────────────────────────

    #[test]
    fn mageos_1x_picks_81_to_83() {
        let composer =
            r#"{"require":{"mage-os/product-community-edition":"1.0.0"}}"#;
        let (c, _raw, src) = magento_default(composer).unwrap();
        assert!(src.contains("mage-os/product-community-edition"));
        assert!(c.matches(&parses("8.1.10")));
        assert!(c.matches(&parses("8.2.10")));
        assert!(c.matches(&parses("8.3.5")));
        assert!(!c.matches(&parses("8.0.30")));
        assert!(!c.matches(&parses("8.4.0")));
    }

    #[test]
    fn mageos_2x_picks_82_to_84() {
        let composer =
            r#"{"require":{"mage-os/product-community-edition":"2.0.0"}}"#;
        let (c, _raw, src) = magento_default(composer).unwrap();
        assert!(src.contains("2.0"));
        assert!(c.matches(&parses("8.2.10")));
        assert!(c.matches(&parses("8.3.5")));
        assert!(c.matches(&parses("8.4.1")));
        assert!(!c.matches(&parses("8.1.30")));
        assert!(!c.matches(&parses("8.5.0")));
    }

    #[test]
    fn mageos_3x_picks_83_to_85() {
        let composer =
            r#"{"require":{"mage-os/product-community-edition":"3.0.0"}}"#;
        let (c, _raw, src) = magento_default(composer).unwrap();
        assert!(src.contains("3.0"));
        assert!(c.matches(&parses("8.3.5")));
        assert!(c.matches(&parses("8.4.1")));
        assert!(c.matches(&parses("8.5.0")));
        assert!(!c.matches(&parses("8.2.30")));
    }

    #[test]
    fn mageos_base_alias_works() {
        let composer =
            r#"{"require":{"mage-os/magento2-base":"~1.0.0"}}"#;
        let (c, _raw, src) = magento_default(composer).unwrap();
        assert!(src.contains("mage-os/magento2-base"));
        assert!(c.matches(&parses("8.1.20")));
    }

    #[test]
    fn mageos_extensions_returns_recommended_set() {
        let composer =
            r#"{"require":{"mage-os/product-community-edition":"3.0.0"}}"#;
        let (exts, src) = magento_extensions(composer).unwrap();
        assert!(src.contains("3.0"));
        assert!(exts.contains(&"pdo_mysql"));
        assert!(exts.contains(&"intl"));
        assert!(exts.contains(&"opcache"));
    }
}
