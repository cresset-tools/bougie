//! Service instance identity — `(name, version)`.
//!
//! A catalog service is no longer a singleton keyed by name. An
//! **instance** is one running copy, identified by its catalog `name`
//! plus the concrete `version` selected for it. The version keys the
//! runtime state tree (`state/services/<name>/<version>/…`, see
//! [`bougie_paths::Paths`]) and selects the store tarball
//! (`<name>-<version>`, see `store_layout`), so two versions of one
//! service — or a service running alongside a foreign holder of its
//! default port — coexist without colliding on datadir, socket, ledger,
//! or port.
//!
//! Phase 0 defines the type; the supervisor map, IPC, and path wiring
//! adopt it in later phases. Until then a service runs at its catalog
//! default version, i.e. `Instance::new(name, entry.version)`.

use std::fmt;

/// A resolved running instance: a catalog service name plus its
/// concrete version.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Instance {
    /// Catalog service name (`opensearch`, `mariadb`, …). Stable across
    /// versions.
    pub name: String,
    /// Concrete resolved version (`2.19.5`), never a range or `"*"`.
    pub version: String,
}

impl Instance {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self { name: name.into(), version: version.into() }
    }

    /// Store tarball / canonical id: `<name>-<version>`
    /// (e.g. `opensearch-2.19.5`). Matches the `store/<tarball>/` layout
    /// so the runtime identity and the on-disk binaries share one key.
    #[must_use]
    pub fn tarball(&self) -> String {
        format!("{}-{}", self.name, self.version)
    }

    /// The version path component under `state/services/<name>/`.
    /// Its own method so the encoding stays in one place if a version
    /// string ever needs sanitising for the filesystem.
    #[must_use]
    pub fn dir_segment(&self) -> &str {
        &self.version
    }

    /// Stable map/display key, same bytes as [`Self::tarball`].
    #[must_use]
    pub fn id(&self) -> InstanceId {
        InstanceId(self.tarball())
    }
}

impl fmt::Display for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `name-version` — the same string users see in the store and in
        // `service status`.
        write!(f, "{}-{}", self.name, self.version)
    }
}

/// Owned, hashable key for the supervisor's instance map. Wraps the
/// `<name>-<version>` string so a `HashMap<InstanceId, _>` can replace
/// the old `HashMap<&'static str, _>` keyed on the catalog name alone.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InstanceId(String);

impl InstanceId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&Instance> for InstanceId {
    fn from(inst: &Instance) -> Self {
        inst.id()
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn tarball_and_dir_segment_derive_from_name_version() {
        let inst = Instance::new("opensearch", "2.19.5");
        assert_eq!(inst.tarball(), "opensearch-2.19.5");
        assert_eq!(inst.dir_segment(), "2.19.5");
        assert_eq!(inst.to_string(), "opensearch-2.19.5");
        assert_eq!(inst.id().as_str(), "opensearch-2.19.5");
    }

    #[test]
    fn id_is_a_usable_map_key_distinguishing_versions() {
        let a = Instance::new("elasticsearch", "7.17.0");
        let b = Instance::new("elasticsearch", "8.13.4");
        let mut map: HashMap<InstanceId, u16> = HashMap::new();
        map.insert(a.id(), 9200);
        map.insert(b.id(), 9201);
        assert_eq!(map.len(), 2, "two versions of one service are distinct keys");
        assert_eq!(map[&a.id()], 9200);
        assert_eq!(map[&b.id()], 9201);
        // Same (name, version) → same key.
        assert_eq!(Instance::new("elasticsearch", "7.17.0").id(), a.id());
    }
}
