//! Packagist v2 metadata types.
//!
//! A `/p2/<vendor>/<name>.json` document lists every published version
//! of a single package. Packagist serves it in *minified* form
//! (`"minified": "composer/2.0"`): the first version is fully expanded
//! and every subsequent version is a sparse diff against the previous,
//! where a JSON `null` resets a key to "not set". Composer's reference
//! implementation lives at
//! `Composer\MetadataMinifier\MetadataMinifier::expand`. We mirror it
//! here on raw `serde_json::Value`s — `LockPackage` is typed and would
//! reject `"autoload": null` directly, so we must apply the diff at
//! the JSON layer first and then deserialize the result.
//!
//! Non-minified responses (no `minified` field, or any value other
//! than `composer/2.0`) are returned as-is: each entry stands alone.
//!
//! Once expanded each version is the same Composer-package-schema
//! shape that ends up in `composer.lock`'s `packages` array, so we
//! reuse [`crate::lockfile::LockPackage`] for the typed result rather
//! than introducing a parallel struct that would drift over time.

use crate::lockfile::LockPackage;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Raw Packagist v2 response, before minified-expansion. Versions are
/// kept as `serde_json::Map`s so the expansion pass can apply the
/// `composer/2.0` diff algorithm before any typed deserialization.
#[derive(Debug, Clone, Deserialize)]
pub struct RawPackageDocument {
    #[serde(default)]
    pub minified: Option<String>,
    /// `packages` is a single-entry map keyed by `vendor/name` whose
    /// value is the ordered list of version objects. Packagist always
    /// sorts newest-first.
    #[serde(default)]
    pub packages: BTreeMap<String, Vec<Map<String, Value>>>,
}

/// Expanded `/p2/` document: each package's version list contains
/// fully-materialized `LockPackage` entries with inheritance applied.
#[derive(Debug, Clone)]
pub struct PackageMetadata {
    /// Maps `vendor/name` to its version list (newest-first, matching
    /// Packagist's output order).
    pub packages: BTreeMap<String, Vec<LockPackage>>,
}

impl PackageMetadata {
    /// Parse a `/p2/` JSON body and apply minified-expansion.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let raw: RawPackageDocument =
            serde_json::from_slice(bytes).wrap_err("parsing Packagist v2 metadata")?;
        raw.expand()
    }
}

impl RawPackageDocument {
    /// Apply the `composer/2.0` minified-expansion algorithm (if
    /// declared) and deserialize each version into a [`LockPackage`].
    pub fn expand(self) -> Result<PackageMetadata> {
        let minified = self.minified.as_deref() == Some("composer/2.0");
        let mut out: BTreeMap<String, Vec<LockPackage>> = BTreeMap::new();
        for (name, versions) in self.packages {
            let mut expanded: Vec<LockPackage> = Vec::with_capacity(versions.len());
            // Running accumulator: the "previous version" each diff is
            // applied against. Only used when `minified` is true.
            let mut acc: Map<String, Value> = Map::new();
            for v in versions {
                let materialized = if minified {
                    apply_diff(&mut acc, v);
                    acc.clone()
                } else {
                    v
                };
                let pkg: LockPackage = serde_json::from_value(Value::Object(materialized))
                    .wrap_err_with(|| {
                        format!("deserializing Packagist v2 version entry for {name}")
                    })?;
                expanded.push(pkg);
            }
            out.insert(name, expanded);
        }
        Ok(PackageMetadata { packages: out })
    }
}

/// Apply one minified-diff entry on top of the running accumulator:
/// JSON `null` values remove a key (resetting inheritance); any other
/// value overwrites it.
fn apply_diff(acc: &mut Map<String, Value>, diff: Map<String, Value>) {
    for (k, v) in diff {
        if v.is_null() {
            acc.remove(&k);
        } else {
            acc.insert(k, v);
        }
    }
}

#[cfg(test)]
mod tests;
