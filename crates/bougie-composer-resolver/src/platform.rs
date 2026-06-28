//! Runtime platform facts the resolver validates package `require`
//! clauses against — cresset-tools/bougie#118.
//!
//! Composer treats `php`, `ext-*`, `lib-*`, `hhvm`, and the
//! `composer*` API packages as *platform packages*: virtual packages
//! whose single version is fixed by the runtime rather than fetched
//! from a repository. Before this type existed, both providers dropped
//! every platform requirement (see [`crate::verify::provider::is_platform`]),
//! so a dependency that needs PHP 8.4 would be locked even under a
//! project pinned to PHP 8.3 — and the resulting lock then fails to
//! install under that PHP.
//!
//! Scope today is `php` only. The pinned PHP version becomes the single
//! candidate for the `php` (and `php-64bit`) platform package, so a
//! `require` whose constraint excludes it makes the solve fail with a
//! proper derivation tree instead of silently resolving.
//!
//! Deliberately still unmodeled — their edges keep being dropped, so
//! there is no behavior change for them:
//!   - `ext-*` / `lib-*`: would need the loaded-extension set, which
//!     `bougie sync` installs *after* resolving (driven by the same
//!     composer.json), so enforcing them at solve time would reject
//!     extensions the very same run is about to add. Needs its own
//!     design.
//!   - `composer` / `composer-plugin-api` / `composer-runtime-api`:
//!     need the running Composer's real API versions; hardcoding them
//!     risks rejecting packages the shipped phar would accept.
//!   - `hhvm` and build-flag `php-*` (`php-zts`, `php-debug`, …).

use std::path::Path;

use composer_semver::version::Version;
use serde_json::Value;

/// Composer-compatible platform-requirement ignore filter — the
/// resolve-time half of `--ignore-platform-reqs` /
/// `--ignore-platform-req=<name>`.
///
/// Mirrors Composer's `IgnoreListPlatformRequirementFilter`: only
/// *platform* packages can be ignored, names match case-insensitively,
/// and `*` is the sole wildcard (`ext-*`, `php*`, bare `*`). An ignored
/// platform package is treated as unmodeled — its requirement edge is
/// dropped during resolution, exactly the pre-#118 behavior — so a
/// constraint the runtime can't satisfy no longer fails the solve.
///
/// `php` (exact) ignores only `php`, not `php-64bit`, matching Composer's
/// `^php$` anchoring; use `php*` to cover both.
#[derive(Debug, Clone, Default)]
pub struct PlatformIgnore {
    /// `--ignore-platform-reqs`: ignore every platform requirement.
    ignore_all: bool,
    /// `--ignore-platform-req=<pattern>` entries, lower-cased.
    patterns: Vec<String>,
}

impl PlatformIgnore {
    /// Build from the CLI flags: `ignore_all` is `--ignore-platform-reqs`,
    /// `patterns` are the `--ignore-platform-req` values (wildcards kept
    /// verbatim, lower-cased for case-insensitive matching).
    pub fn new(ignore_all: bool, patterns: &[String]) -> Self {
        Self {
            ignore_all,
            patterns: patterns.iter().map(|p| p.to_ascii_lowercase()).collect(),
        }
    }

    /// `true` when nothing is ignored (the common case). Lets callers
    /// skip building filtered state.
    pub fn is_empty(&self) -> bool {
        !self.ignore_all && self.patterns.is_empty()
    }

    /// Whether requirement edges to platform package `name` should be
    /// dropped. Only platform packages are ignorable (Composer semantics);
    /// a non-platform name never matches even under `--ignore-platform-reqs`.
    pub fn is_ignored(&self, name: &str) -> bool {
        if !crate::verify::is_platform(name) {
            return false;
        }
        if self.ignore_all {
            return true;
        }
        let lower = name.to_ascii_lowercase();
        self.patterns.iter().any(|p| glob_match(p, &lower))
    }
}

/// Composer's platform-name glob: `*` matches any (possibly empty) run of
/// characters; every other character is literal; the whole name must
/// match end-to-end. Both `pattern` and `name` are expected lower-cased.
/// Mirrors `BasePackage::packageNameToRegexp` (`\*` → `.*`, anchored).
fn glob_match(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    let segments: Vec<&str> = pattern.split('*').collect();
    let last = segments.len() - 1;
    // The first segment is a required prefix.
    let Some(mut rest) = name.strip_prefix(segments[0]) else {
        return false;
    };
    for (i, seg) in segments.iter().enumerate().skip(1) {
        if i == last {
            // The final segment must match the tail; `*` before it
            // absorbs whatever is left.
            return rest.ends_with(seg);
        }
        if seg.is_empty() {
            continue;
        }
        match rest.find(seg) {
            Some(idx) => rest = &rest[idx + seg.len()..],
            None => return false,
        }
    }
    true
}

/// Platform package versions for one resolve/verify pass.
///
/// The default value models nothing (every [`Self::candidate`] is
/// `None`), which reproduces the pre-#118 "drop all platform edges"
/// behavior — used by [`crate::update::ResolveProvider::build`] and any
/// caller that doesn't supply a PHP version.
#[derive(Debug, Clone, Default)]
pub struct PlatformEnv {
    /// The project's resolved PHP version. `None` when it can't be
    /// determined (e.g. an un-synced project), in which case `php`
    /// requirements are left unvalidated rather than failing on
    /// incomplete data.
    php: Option<Version>,
    /// Platform requirements to treat as always-satisfied (drop the edge).
    /// Empty by default; populated from `--ignore-platform-req(s)`.
    ignore: PlatformIgnore,
}

impl PlatformEnv {
    /// Construct with an explicit PHP version. `None` models no
    /// platform packages.
    pub fn new(php: Option<Version>) -> Self {
        Self { php, ignore: PlatformIgnore::default() }
    }

    /// Attach an ignore filter (from `--ignore-platform-req(s)`). Ignored
    /// platform packages report no candidate, so their requirement edges
    /// are dropped during resolution.
    #[must_use]
    pub fn ignoring(mut self, ignore: PlatformIgnore) -> Self {
        self.ignore = ignore;
        self
    }

    /// Best-effort detection of the project's PHP version, preferring
    /// the exact resolved pin written by `sync`
    /// (`vendor/bougie/state/resolved`) and falling back to the *declared*
    /// pin in composer.json (`extra.bougie.php.version`). The fallback
    /// matters because `bougie php pin` records the declared pin in
    /// composer.json immediately, but the resolved marker only appears
    /// after the first sync — without it, a fresh project's first
    /// resolve would skip PHP validation. Any failure yields an env
    /// that models nothing — never an error.
    pub fn detect(project_root: &Path, composer_json: &Value) -> Self {
        let php = read_resolved_pin(project_root).or_else(|| read_declared_pin(composer_json));
        Self { php, ignore: PlatformIgnore::default() }
    }

    /// Like [`Self::detect`] but reads only the resolved marker — used
    /// where the parsed composer.json isn't on hand.
    pub fn from_project(project_root: &Path) -> Self {
        Self { php: read_resolved_pin(project_root), ignore: PlatformIgnore::default() }
    }

    /// The single candidate version for platform package `name`, or
    /// `None` if bougie doesn't model it. When `None`, callers drop the
    /// requirement edge, leaving it unconstrained exactly as before.
    pub fn candidate(&self, name: &str) -> Option<Version> {
        // An ignored platform requirement reports no candidate, so the
        // edge is dropped (unconstrained) exactly as for an unmodeled
        // package — that is what "ignore this platform req" means.
        if self.ignore.is_ignored(name) {
            return None;
        }
        match name {
            // `php-64bit` tracks the PHP version on 64-bit builds,
            // which every bougie-managed interpreter is.
            "php" | "php-64bit" => self.php.clone(),
            _ => None,
        }
    }

    /// Whether `name` is a platform package bougie validates against
    /// the runtime. When `false`, requirement edges to `name` are
    /// dropped during graph construction (unconstrained, as before).
    pub fn models(&self, name: &str) -> bool {
        self.candidate(name).is_some()
    }
}

/// Read the exact resolved PHP version from
/// `vendor/bougie/state/resolved` (mirrors
/// `bougie_fs::state::read_project_resolved`, inlined to avoid a crate
/// dependency). The marker is `<version>-<flavor>`, e.g. `8.3.31-nts`;
/// we keep the version up to the first `-`.
fn read_resolved_pin(project_root: &Path) -> Option<Version> {
    let body =
        std::fs::read_to_string(bougie_paths::project::resolved(project_root)).ok()?;
    let line = body.trim();
    let version = line.split('-').next().unwrap_or(line);
    Version::parse(version).ok()
}

/// Read the declared PHP pin from composer.json's
/// `extra.bougie.php.version` (what `bougie php pin` writes — typically
/// a minor like `"8.3"`, which parses to `8.3.0`).
fn read_declared_pin(composer_json: &Value) -> Option<Version> {
    let raw = composer_json
        .get("extra")?
        .get("bougie")?
        .get("php")?
        .get("version")?
        .as_str()?;
    Version::parse(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn models_php_when_version_known() {
        let env = PlatformEnv::new(Some(v("8.3.31")));
        assert_eq!(env.candidate("php"), Some(v("8.3.31")));
        assert_eq!(env.candidate("php-64bit"), Some(v("8.3.31")));
        assert!(env.models("php"));
    }

    #[test]
    fn unmodeled_platform_packages_have_no_candidate() {
        let env = PlatformEnv::new(Some(v("8.3.31")));
        for name in ["ext-intl", "lib-curl", "composer-runtime-api", "hhvm", "php-zts"] {
            assert_eq!(env.candidate(name), None, "{name} must stay unmodeled");
            assert!(!env.models(name), "{name} must stay unmodeled");
        }
    }

    #[test]
    fn unknown_php_models_nothing() {
        let env = PlatformEnv::default();
        assert_eq!(env.candidate("php"), None);
        assert!(!env.models("php"));
    }

    #[test]
    fn from_project_reads_resolved_pin() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("vendor").join("bougie").join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("resolved"), "8.3.31-nts\n").unwrap();
        let env = PlatformEnv::from_project(tmp.path());
        assert_eq!(env.candidate("php"), Some(v("8.3.31")));
    }

    #[test]
    fn from_project_without_pin_models_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let env = PlatformEnv::from_project(tmp.path());
        assert!(!env.models("php"));
    }

    #[test]
    fn detect_falls_back_to_declared_pin_in_composer_json() {
        // No resolved marker on disk (un-synced project), but the
        // declared pin in composer.json is present — `detect` uses it.
        let tmp = tempfile::TempDir::new().unwrap();
        let composer_json = serde_json::json!({
            "extra": { "bougie": { "php": { "version": "8.3" } } }
        });
        let env = PlatformEnv::detect(tmp.path(), &composer_json);
        assert_eq!(env.candidate("php"), Some(v("8.3")));
    }

    #[test]
    fn glob_match_handles_composer_wildcards() {
        // Exact match (no wildcard).
        assert!(glob_match("php", "php"));
        assert!(!glob_match("php", "php-64bit"));
        // Trailing wildcard.
        assert!(glob_match("php*", "php"));
        assert!(glob_match("php*", "php-64bit"));
        assert!(glob_match("ext-*", "ext-intl"));
        assert!(!glob_match("ext-*", "php"));
        // Bare `*` matches anything.
        assert!(glob_match("*", "ext-intl"));
        assert!(glob_match("*", "php"));
        // Internal wildcard.
        assert!(glob_match("ext-*intl", "ext-pdo-intl"));
        assert!(!glob_match("ext-*intl", "ext-pdo-mysql"));
    }

    #[test]
    fn ignore_only_matches_platform_packages() {
        let ig = PlatformIgnore::new(false, &["*".to_string()]);
        assert!(ig.is_ignored("php"));
        assert!(ig.is_ignored("ext-intl"));
        // `*` is broad, but non-platform packages are never ignorable.
        assert!(!ig.is_ignored("acme/foo"));
    }

    #[test]
    fn ignore_all_covers_every_platform_package() {
        let ig = PlatformIgnore::new(true, &[]);
        assert!(ig.is_ignored("php"));
        assert!(ig.is_ignored("php-64bit"));
        assert!(ig.is_ignored("ext-gd"));
        assert!(!ig.is_ignored("acme/foo"));
        assert!(!ig.is_empty());
    }

    #[test]
    fn ignore_php_exact_does_not_cover_php_64bit() {
        let ig = PlatformIgnore::new(false, &["php".to_string()]);
        assert!(ig.is_ignored("php"));
        assert!(!ig.is_ignored("php-64bit"));
    }

    #[test]
    fn ignored_php_reports_no_candidate() {
        let env = PlatformEnv::new(Some(v("8.3.31")))
            .ignoring(PlatformIgnore::new(false, &["php".to_string()]));
        // `php` is ignored → no candidate → edge dropped during resolve.
        assert_eq!(env.candidate("php"), None);
        assert!(!env.models("php"));
        // `php-64bit` is still modeled (exact `php` doesn't cover it).
        assert_eq!(env.candidate("php-64bit"), Some(v("8.3.31")));
    }

    #[test]
    fn empty_ignore_is_a_no_op() {
        let ig = PlatformIgnore::default();
        assert!(ig.is_empty());
        assert!(!ig.is_ignored("php"));
        let env = PlatformEnv::new(Some(v("8.3.31"))).ignoring(ig);
        assert_eq!(env.candidate("php"), Some(v("8.3.31")));
    }

    #[test]
    fn detect_prefers_resolved_marker_over_declared_pin() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("vendor").join("bougie").join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("resolved"), "8.3.31-nts\n").unwrap();
        let composer_json = serde_json::json!({
            "extra": { "bougie": { "php": { "version": "8.2" } } }
        });
        // Resolved marker (8.3.31) wins over the coarser declared pin.
        let env = PlatformEnv::detect(tmp.path(), &composer_json);
        assert_eq!(env.candidate("php"), Some(v("8.3.31")));
    }
}
