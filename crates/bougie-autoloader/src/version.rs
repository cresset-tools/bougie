//! Port of `Composer\Semver\VersionParser::normalize` from
//! composer/semver 3.4.4 (shipped inside composer-2.8.12.phar).
//!
//! `installed.json`'s `version_normalized` and `installed.php`'s
//! `version` field both come from this function. Ground-truth outputs
//! for ~50 inputs are captured in
//! `tests/data/version_normalize.tsv` and exercised by the
//! `tests/version_normalize.rs` integration harness.
//!
//! **Current scope:** Minimal subset — pad `X.Y.Z` to `X.Y.Z.0`, strip
//! a leading `v`, drop `+build` metadata. The full
//! pre-release-modifier / dev-branch / date-version / aliasing rules
//! Composer applies are unimplemented; the harness flags every case
//! that diverges, and the missing branches land in follow-ups.

/// Normalize a version string the way Composer's
/// `Composer\Semver\VersionParser::normalize` does. Returns the
/// canonical normalized form (e.g. `"1.0.0"` → `"1.0.0.0"`,
/// `"1.0.0-RC1"` → `"1.0.0.0-RC1"`).
///
/// **Incomplete port** — see module docs. Callers feeding lockfile
/// versions get correct output for the cases the path-repo fixtures
/// exercise; other inputs may be silently mangled until the full
/// regex-driven implementation lands.
pub(crate) fn normalize(s: &str) -> String {
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
