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
    // `exclude-from-classmap` deserializes but is not yet honored;
    // wired up alongside `--optimize` in a follow-up PR.
    #[serde(default, rename = "exclude-from-classmap")]
    #[allow(dead_code)]
    pub exclude_from_classmap: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RootManifest {
    #[serde(default)]
    pub autoload: AutoloadBlock,
    #[serde(default, rename = "autoload-dev")]
    #[allow(dead_code)] // wired in once dev separation matters (Phase 3)
    pub autoload_dev: AutoloadBlock,
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
}

pub(crate) fn read_lock(project_root: &Path) -> Result<LockFile, DumpError> {
    let path = project_root.join("composer.lock");
    let bytes = std::fs::read(&path)?;
    serde_json::from_slice(&bytes).map_err(|e| DumpError::Lock(format!("{path:?}: {e}")))
}

pub(crate) fn read_root_manifest(project_root: &Path) -> Result<RootManifest, DumpError> {
    let path = project_root.join("composer.json");
    let bytes = std::fs::read(&path)?;
    serde_json::from_slice(&bytes).map_err(|e| DumpError::Manifest(format!("{path:?}: {e}")))
}

/// Composer's PSR-4 / PSR-0 maps accept either a single string or an
/// array of strings as the value. Both shapes get normalized to
/// `Vec<String>`. Order is preserved (we requested `preserve_order`
/// from serde_json at the crate level).
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
