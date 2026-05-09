//! Project + global configuration per CLI.md §4.
//!
//! `composer.json`'s `require.php` / `require.ext-*` / `extra.bougie`
//! are read here; `bougie.toml` is read here. The merge between the two
//! follows §4.2.1.

mod composer;
mod merge;
mod toml;

pub use composer::{read_composer_json, ComposerJson};
pub use merge::{load_project, merge, ProjectConfig};
pub use toml::{read_bougie_toml, write_bougie_toml_skeleton};

use serde::Deserialize;
use std::collections::BTreeMap;

/// The bougie-specific configuration block. Lives either in
/// `bougie.toml` (top level) or under `composer.json`'s
/// `extra.bougie`. Both forms deserialize into this struct.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BougieConfig {
    pub php: PhpConfig,
    pub extensions: BTreeMap<String, String>,
    pub index: Vec<IndexEntry>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PhpConfig {
    pub version: Option<String>,
    pub flavor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IndexEntry {
    pub host: String,
    pub fingerprint: String,
}
