//! Shared `FxHasher`-backed map / set aliases for the resolver's hot
//! paths.
//!
//! The pubgrub solve loop pounds these maps with `vendor/name` string
//! keys — `versions_for` lookups, `merged_cache` reads, the parsed-deps
//! cache from PR 1, the virtual-provider index. The default `SipHash`
//! is overkill for those: DoS resistance buys nothing against a fixed,
//! locally-loaded fixture, and `FxHasher` is several times faster on
//! the short string keys the solver actually uses.
//!
//! Mirrors uv's choice (see `uv-resolver/src/resolver/index.rs`) —
//! `FxHasher` on every map pubgrub touches.

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasherDefault;

pub use rustc_hash::FxHasher;

pub type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;
pub type FxHashSet<K> = HashSet<K, BuildHasherDefault<FxHasher>>;
