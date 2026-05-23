//! Interned `vendor/name` strings for the pubgrub solve.
//!
//! The same `vendor/name` lives in seven of the resolver's caches +
//! every `PubGrubPackage::Package(_)` pubgrub clones around during
//! conflict analysis. Owning `String`s everywhere costs an allocator
//! call per clone and inflates each map's working set. Interning
//! collapses that to a refcount bump per clone.
//!
//! Mirrors uv's `PackageName(ArcStr)` design
//! (`uv-normalize::PackageName` wraps `arcstr::ArcStr`). The wrapper
//! exists so future invariants — case-normalization, validation,
//! length checks — have one place to land; today it's a transparent
//! pass-through.
//!
//! `PackageName` lives at the resolver boundary. `LockPackage` (a
//! foreign, `Deserialize`-bound type) keeps its `String` field;
//! callers intern once when they hand a name into the resolver:
//! `load_real_candidates`, `compute_virtual_contributions`,
//! `read_root_requires`.
//!
//! `Borrow<str>` is the load-bearing trait — it's what lets
//! `HashMap<PackageName, V>::get(s: &str)` work without allocating a
//! fresh `PackageName` per lookup.

use std::borrow::Borrow;
use std::fmt;

use arcstr::ArcStr;

/// A `vendor/name` package identifier, interned via `Arc<str>`.
/// Clones are refcount bumps.
///
/// `Ord` + `PartialOrd` delegate to the inner string so `PackageName`
/// can sit in a `BTreeMap` and round-trip the lexicographic grouping
/// the resolver depends on (`compute_parsed_deps` groups virtual
/// providers by name, and `register_virtuals_from` callers rely on
/// `PrefetchOutcome::name` sorting alphabetically for deterministic
/// virtual-index registration order).
#[derive(Clone, Eq, PartialEq, Hash, Debug, PartialOrd, Ord)]
pub struct PackageName(ArcStr);

impl PackageName {
    /// Borrow as a `&str`. Cheaper than `.as_ref()` in spots that
    /// already have the `PackageName` and want to forward to APIs
    /// that take `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PackageName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for PackageName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PackageName {
    fn from(s: &str) -> Self {
        Self(ArcStr::from(s))
    }
}

impl From<String> for PackageName {
    fn from(s: String) -> Self {
        Self(ArcStr::from(s))
    }
}

impl From<&String> for PackageName {
    fn from(s: &String) -> Self {
        Self(ArcStr::from(s.as_str()))
    }
}

impl From<ArcStr> for PackageName {
    fn from(s: ArcStr) -> Self {
        Self(s)
    }
}
