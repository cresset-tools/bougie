//! Packagist v2 metadata → typed [`LockPackage`] lists.
//!
//! The Composer v2 repository wire format — the `p2/*.json` document and
//! its `composer/2.0` minified-diff algorithm — lives in the shared
//! [`composer_wire`] crate. This module is the thin bougie-side adapter:
//! parse + expand via `composer-wire`, then deserialize each
//! fully-materialized version object into bougie's typed [`LockPackage`]
//! so the resolver keeps working with one struct.

use crate::lockfile::LockPackage;
use composer_wire::PackageDocument;
use eyre::{Result, WrapErr};
use serde_json::Value;
use std::collections::BTreeMap;

/// Expanded `/p2/` document: each package's version list materialized
/// into typed [`LockPackage`] entries (newest-first, matching Packagist's
/// output order).
#[derive(Debug, Clone)]
pub struct PackageMetadata {
    /// Maps `vendor/name` to its version list.
    pub packages: BTreeMap<String, Vec<LockPackage>>,
}

impl PackageMetadata {
    /// Parse a `/p2/` JSON body: apply the `composer/2.0` minified-
    /// expansion (via [`composer_wire`]) and deserialize each
    /// fully-materialized version into a [`LockPackage`].
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let expanded = PackageDocument::parse(bytes)
            .wrap_err("parsing Packagist v2 metadata")?
            .expand();
        let mut packages: BTreeMap<String, Vec<LockPackage>> = BTreeMap::new();
        for (name, versions) in expanded {
            let mut typed = Vec::with_capacity(versions.len());
            for version in versions {
                let pkg: LockPackage =
                    serde_json::from_value(Value::Object(version)).wrap_err_with(|| {
                        format!("deserializing Packagist v2 version entry for {name}")
                    })?;
                typed.push(pkg);
            }
            packages.insert(name, typed);
        }
        Ok(Self { packages })
    }
}

#[cfg(test)]
mod tests;
