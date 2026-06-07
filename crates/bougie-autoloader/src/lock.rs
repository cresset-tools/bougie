//! Lightweight readers for `composer.lock` and root `composer.json`.
//!
//! Scope: only the fields the autoloader cares about (content-hash,
//! per-package autoload blocks, package names, dev-vs-prod split).
//! Once `bougie-composer-resolver` lands, this is the natural place
//! to lift these readers up to a shared crate; for now keeping it
//! inline keeps `bougie-autoloader` independently testable.

use std::path::Path;

use serde::Deserialize;

use crate::DumpError;

#[derive(Debug, Deserialize)]
pub(crate) struct LockFile {
    #[serde(rename = "content-hash")]
    pub content_hash: String,
    #[serde(default)]
    pub packages: Vec<Package>,
    #[serde(default, rename = "packages-dev")]
    pub packages_dev: Vec<Package>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Package {
    pub name: String,
    /// Package `type`, drives `composer/installers` path remapping
    /// (`bougie-installers::install_path`). Most packages are
    /// `"library"` (or omit it); only the handful of relocatable types
    /// move out of `vendor/<name>`.
    #[serde(default, rename = "type")]
    pub package_type: Option<String>,
    #[serde(default)]
    pub autoload: AutoloadBlock,
    /// Other packages this one requires. Composer's `PackageSorter`
    /// uses this to build a usage graph for topological-ish sorting
    /// (see `LockFile::reverse_sorted_packages`, which only reads the
    /// keys). The constraint *values* additionally feed
    /// `platform_check.php` generation — the `php` / `php-64bit` /
    /// `ext-*` requirements across every prod package determine the
    /// emitted version + extension guards.
    #[serde(default)]
    pub require: std::collections::BTreeMap<String, String>,
    /// Packages/platform packages this one `replace`s. Together with
    /// [`Package::provide`] this is how a polyfill declares it stands in
    /// for an `ext-*` requirement, so the platform check skips emitting
    /// an `extension_loaded()` guard for it (Composer's
    /// `$extensionProviders`). Values are constraints.
    #[serde(default)]
    pub replace: std::collections::BTreeMap<String, String>,
    /// Platform/virtual packages this one `provide`s — see
    /// [`Package::replace`].
    #[serde(default)]
    pub provide: std::collections::BTreeMap<String, String>,
    /// `dist` block — only the `type` discriminant is read. Path-repo
    /// packages (`dist.type == "path"`) need their classmap scan roots
    /// added to the user-code watcher set so live patches see changes
    /// inside those directories. Other kinds (zip, tar, …) live in
    /// `vendor/` proper and are covered by the `composer.lock` watcher.
    #[serde(default)]
    pub dist: Option<LockDist>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LockDist {
    #[serde(default, rename = "type")]
    pub kind: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct AutoloadBlock {
    #[serde(default, rename = "psr-4", deserialize_with = "de_namespace_map")]
    pub psr4: Vec<(String, Vec<String>)>,
    #[serde(default, rename = "psr-0", deserialize_with = "de_namespace_map")]
    pub psr0: Vec<(String, Vec<String>)>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub classmap: Vec<String>,
    #[serde(default, rename = "exclude-from-classmap")]
    pub exclude_from_classmap: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RootManifest {
    #[serde(default)]
    pub autoload: AutoloadBlock,
    /// Root-package requirements. The root is the first entry in
    /// Composer's `$packageMap`, so its `php` / `ext-*` requires feed
    /// `platform_check.php` alongside the locked packages'.
    #[serde(default)]
    pub require: std::collections::BTreeMap<String, String>,
    /// Root `replace` / `provide` — completes the extension-provider set
    /// for the platform check (rare on a root package, but Composer
    /// includes it).
    #[serde(default)]
    pub replace: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub provide: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "autoload-dev")]
    #[allow(dead_code)] // wired in once dev separation matters (Phase 3)
    pub autoload_dev: AutoloadBlock,
    /// `config` block. Only the fields the autoloader cares about are
    /// extracted; everything else is dropped.
    #[serde(default)]
    pub config: RootConfig,
    /// `extra` block, kept as raw JSON so the `composer/installers`
    /// `installer-paths` overrides can be parsed
    /// (`bougie-installers::InstallerPaths::from_extra`). Everything
    /// else under `extra` is ignored.
    #[serde(default)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RootConfig {
    /// `autoloader-suffix` override — when set, replaces the
    /// `composer.lock` `content-hash` as the
    /// `ComposerAutoloaderInit<X>` / `ComposerStaticInit<X>` class
    /// suffix. Lets the user stabilize the suffix across
    /// content-hash-changing edits.
    #[serde(default, rename = "autoloader-suffix")]
    pub autoloader_suffix: Option<String>,
    /// `platform-check` — whether to emit `platform_check.php` and how
    /// strict it is. Composer's default is `"php-only"`, so this field
    /// defaults to [`PlatformCheck::PhpOnly`] when the key is absent.
    #[serde(default, rename = "platform-check")]
    pub platform_check: PlatformCheck,
}

/// `config.platform-check`. Composer accepts a bool or the string
/// `"php-only"`; `false` disables the check, `true` emits both the PHP
/// version guard and per-extension `extension_loaded()` guards, and
/// `"php-only"` (the default) emits only the version guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PlatformCheck {
    /// `false` — no `platform_check.php` at all.
    Disabled,
    /// `"php-only"` (Composer's default) — PHP version guard only.
    #[default]
    PhpOnly,
    /// `true` — PHP version guard plus extension guards.
    Strict,
}

impl<'de> Deserialize<'de> for PlatformCheck {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Str(String),
        }
        Ok(match Raw::deserialize(d)? {
            Raw::Bool(true) => PlatformCheck::Strict,
            Raw::Bool(false) => PlatformCheck::Disabled,
            // Composer only blesses "php-only"; treat any other string
            // the lenient way (its documented default behavior) rather
            // than failing the whole dump.
            Raw::Str(s) if s == "php-only" => PlatformCheck::PhpOnly,
            Raw::Str(_) => PlatformCheck::PhpOnly,
        })
    }
}

impl LockFile {
    /// Iterate packages in Composer's emission order: prod packages
    /// first (in their lockfile order — Composer sorts them
    /// alphabetically before writing), then dev packages last when
    /// not skipped.
    pub(crate) fn iter_packages(&self, no_dev: bool) -> impl Iterator<Item = &Package> {
        let dev: &[Package] = if no_dev { &[] } else { &self.packages_dev };
        self.packages.iter().chain(dev.iter())
    }

    /// Iterate packages in Composer's `sortPackageMap` order:
    /// `PackageSorter::sortPackages` — dependencies before
    /// dependents (ascending importance weight, alphabetical
    /// tie-break). Root is handled separately by the caller. Used by
    /// the files-autoload emitter; matches Composer's
    /// `parseAutoloads` line `$files = $this->parseAutoloadsType(
    /// $sortedPackageMap, 'files', ...)` (deps-first so an upstream
    /// package's files autoload runs before any dependent that might
    /// reference its symbols at include time).
    pub(crate) fn sorted_packages(&self, no_dev: bool) -> Vec<&Package> {
        let mut all: Vec<&Package> = self.packages.iter().collect();
        if !no_dev {
            all.extend(self.packages_dev.iter());
        }
        sort_packages(all)
    }

    /// Iterate packages in Composer's `reverseSortedMap` order:
    /// reverse of `PackageSorter::sortPackages`. Root is handled
    /// separately by the caller — Composer's iteration is
    /// `[root, ...reverse(sortPackages(deps))]` so callers should
    /// process root first, then this iterator.
    ///
    /// PSR-*/classmap aggregation, the optimize-mode PSR-* scan, and
    /// the classmap scan all use this ordering — Composer applies
    /// `array_reverse` to `sortPackageMap` output before iterating in
    /// `parseAutoloadsType` and `dump()`. The ordering only affects
    /// output when multiple packages contribute paths or classes to
    /// the same namespace; the fixture `psr4-shared-namespace` is the
    /// minimal case.
    pub(crate) fn reverse_sorted_packages(&self, no_dev: bool) -> Vec<&Package> {
        self.sorted_packages(no_dev).into_iter().rev().collect()
    }
}

/// Port of `Composer\Util\PackageSorter::sortPackages` for our
/// reduced view of the lockfile.
///
/// Algorithm: compute a per-package weight by walking the reverse
/// usage graph (`who requires me` chained recursively). Tie-break
/// alphabetically (Composer uses `strnatcasecmp`; we use plain ASCII
/// `cmp` since real package names are lowercase ASCII).
fn importance<'a>(
    name: &'a str,
    usage: &std::collections::HashMap<&'a str, Vec<&'a str>>,
    // `i64` (not `i32`) to match Composer's PHP-int arithmetic. The
    // weight accumulates `-= 1 - importance(user)` across the whole
    // reverse-dependency graph; on a large, densely cross-requiring
    // project (a full Mage-OS install is ~580 packages) the magnitude
    // exceeds `i32::MIN`, which panicked here in debug builds and
    // wrapped silently in release. PHP ints are 64-bit, so Composer
    // never hit this.
    computed: &mut std::collections::HashMap<&'a str, i64>,
    computing: &mut std::collections::HashSet<&'a str>,
) -> i64 {
    if let Some(&v) = computed.get(name) {
        return v;
    }
    if !computing.insert(name) {
        // cycle — Composer returns 0.
        return 0;
    }
    let mut weight = 0;
    if let Some(users) = usage.get(name) {
        for u in users {
            weight -= 1 - importance(u, usage, computed, computing);
        }
    }
    computing.remove(name);
    computed.insert(name, weight);
    weight
}

fn sort_packages(packages: Vec<&Package>) -> Vec<&Package> {
    use std::collections::HashMap;

    // usage[target] = list of package names that require `target`.
    let mut usage: HashMap<&str, Vec<&str>> = HashMap::new();
    for pkg in &packages {
        for dep_name in pkg.require.keys() {
            usage.entry(dep_name.as_str()).or_default().push(&pkg.name);
        }
    }

    // Recursive weight computation, memoized; cycle-broken by a
    // "computing" guard (matches Composer's $computing array).
    let mut computed: HashMap<&str, i64> = HashMap::new();
    let mut computing: std::collections::HashSet<&str> = std::collections::HashSet::new();

    let mut weighted: Vec<(i64, &Package)> = packages
        .iter()
        .map(|p| {
            (
                importance(&p.name, &usage, &mut computed, &mut computing),
                *p,
            )
        })
        .collect();
    // Stable sort by (weight asc, name asc).
    weighted.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    weighted.into_iter().map(|(_, p)| p).collect()
}

pub(crate) fn read_lock(project_root: &Path) -> Result<LockFile, DumpError> {
    let path = project_root.join("composer.lock");
    let bytes = std::fs::read(&path)?;
    serde_json::from_slice(&bytes).map_err(|e| DumpError::Lock(format!("{}: {e}", path.display())))
}

pub(crate) fn read_root_manifest(project_root: &Path) -> Result<RootManifest, DumpError> {
    let path = project_root.join("composer.json");
    let bytes = std::fs::read(&path)?;
    serde_json::from_slice(&bytes).map_err(|e| DumpError::Manifest(format!("{}: {e}", path.display())))
}

/// Composer's PSR-4 / PSR-0 maps accept either a single string or an
/// array of strings as the value. Both shapes get normalized to
/// `Vec<String>`. Order is preserved (we requested `preserve_order`
/// from `serde_json` at the crate level).
fn de_namespace_map<'de, D>(d: D) -> Result<Vec<(String, Vec<String>)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    let raw: serde_json::Map<String, serde_json::Value> = Deserialize::deserialize(d)?;
    let mut out = Vec::with_capacity(raw.len());
    for (k, v) in raw {
        let parsed: OneOrMany = serde_json::from_value(v).map_err(D::Error::custom)?;
        let vs = match parsed {
            OneOrMany::One(s) => vec![s],
            OneOrMany::Many(v) => v,
        };
        out.push((k, vs));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, requires: &[String]) -> Package {
        let require: serde_json::Map<String, serde_json::Value> = requires
            .iter()
            .map(|r| (r.clone(), serde_json::Value::String("*".to_owned())))
            .collect();
        serde_json::from_value(serde_json::json!({
            "name": name,
            "require": require,
        }))
        .expect("valid Package json")
    }

    #[test]
    fn package_sort_weight_does_not_overflow_on_deep_graph() {
        // Construct a graph whose `PackageSorter` weight grows
        // exponentially: package `p{i}` requires every higher-indexed
        // package, so `usage[p{k}] = {p0..p{k-1}}` and
        // `importance(p{k}) = -(2^k - 1)`. At N=36 the most-depended-on
        // package weighs `-(2^35 - 1)` (~ -3.4e10), which overflows
        // `i32` (panicked in debug / wrapped in release) but fits the
        // `i64` Composer-parity type. A full Mage-OS install (~580
        // densely cross-requiring packages) hit this for real.
        const N: usize = 36;
        let names: Vec<String> = (0..N).map(|i| format!("acme/p{i}")).collect();
        let packages: Vec<Package> = (0..N)
            .map(|i| {
                let requires: Vec<String> = names[i + 1..].to_vec();
                pkg(&names[i], &requires)
            })
            .collect();

        let refs: Vec<&Package> = packages.iter().collect();
        // Must not panic on overflow.
        let sorted = sort_packages(refs);

        assert_eq!(sorted.len(), N, "every package is returned");
        // The foundational package (required by all others, most
        // negative weight) sorts first.
        assert_eq!(
            sorted.first().map(|p| p.name.as_str()),
            Some(format!("acme/p{}", N - 1).as_str()),
            "most-depended-on package sorts first",
        );
    }
}
