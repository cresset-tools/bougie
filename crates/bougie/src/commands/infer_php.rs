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

use bougie_semver::Constraint;
use serde_json::Value;
use std::fs;
use std::path::Path;

/// Try to infer a PHP constraint from on-disk project files. Returns
/// `(constraint, source_label)` or `None`.
pub fn infer(project_root: &Path) -> Option<(Constraint, String)> {
    if let Ok(text) = fs::read_to_string(project_root.join("composer.json"))
        && let Some(found) = magento_default(&text)
    {
        return Some(found);
    }
    if let Ok(text) = fs::read_to_string(project_root.join("composer.lock"))
        && let Some(found) = lockfile_intersection(&text)
    {
        return Some(found);
    }
    None
}

/// Map a magento project to Adobe's officially recommended PHP range.
///
/// We read the constraint on `magento/product-community-edition` (or
/// `magento/magento2-base`), pull out its first written version, and
/// look up the matrix row. Magento constraints are always pinned tight
/// (`^2.4.7`, `~2.4.6`, `2.4.5-p1`), so the first version mention is
/// the deciding lower bound for the matrix.
fn magento_default(composer_json: &str) -> Option<(Constraint, String)> {
    let v: Value = serde_json::from_str(composer_json).ok()?;
    let require = v.get("require").and_then(Value::as_object)?;
    let (pkg, raw) = ["magento/product-community-edition", "magento/magento2-base"]
        .iter()
        .find_map(|k| require.get(*k).and_then(Value::as_str).map(|s| (*k, s)))?;
    let (major, minor, patch) = extract_first_version(raw)?;
    let php = magento_php_for(major, minor, patch)?;
    let c = Constraint::parse(php).ok()?;
    let pv = match patch {
        Some(p) => format!("{major}.{minor}.{p}"),
        None => format!("{major}.{minor}"),
    };
    Some((c, format!("{pkg} {pv}")))
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

#[cfg(test)]
mod tests {
    use super::*;
    use bougie_semver::Version;

    fn parses(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn magento_247_picks_82_or_83() {
        let composer = r#"{"require":{"magento/product-community-edition":"2.4.7-p3"}}"#;
        let (c, src) = magento_default(composer).unwrap();
        assert!(src.contains("2.4.7"));
        assert!(c.matches(&parses("8.2.20")));
        assert!(c.matches(&parses("8.3.10")));
        assert!(!c.matches(&parses("8.1.20")));
        assert!(!c.matches(&parses("8.4.0")));
    }

    #[test]
    fn magento_246_caret_range() {
        let composer = r#"{"require":{"magento/product-community-edition":"^2.4.6"}}"#;
        let (c, _) = magento_default(composer).unwrap();
        assert!(c.matches(&parses("8.1.20")));
        assert!(c.matches(&parses("8.2.20")));
        assert!(c.matches(&parses("8.3.10")));
        assert!(!c.matches(&parses("8.0.30")));
        assert!(!c.matches(&parses("8.4.0")));
    }

    #[test]
    fn magento_base_alias_works() {
        let composer = r#"{"require":{"magento/magento2-base":"~2.4.5"}}"#;
        let (c, src) = magento_default(composer).unwrap();
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
}
