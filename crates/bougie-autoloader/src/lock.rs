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
    #[serde(default)]
    pub autoload: AutoloadBlock,
    /// Other packages this one requires. Composer's `PackageSorter`
    /// uses this to build a usage graph for topological-ish sorting;
    /// see `LockFile::reverse_sorted_packages`. Values are version
    /// constraints we don't care about — only the keys matter.
    #[serde(default)]
    pub require: std::collections::BTreeMap<String, String>,
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
    #[serde(default, rename = "autoload-dev")]
    #[allow(dead_code)] // wired in once dev separation matters (Phase 3)
    pub autoload_dev: AutoloadBlock,
    /// `config` block. Only the fields the autoloader cares about are
    /// extracted; everything else is dropped.
    #[serde(default)]
    pub config: RootConfig,
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
    computed: &mut std::collections::HashMap<&'a str, i32>,
    computing: &mut std::collections::HashSet<&'a str>,
) -> i32 {
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
    let mut computed: HashMap<&str, i32> = HashMap::new();
    let mut computing: std::collections::HashSet<&str> = std::collections::HashSet::new();

    let mut weighted: Vec<(i32, &Package)> = packages
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
