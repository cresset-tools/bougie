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
    pub composer: ComposerConfig,
    pub extensions: BTreeMap<String, ExtensionPin>,
    pub services: BTreeMap<String, ServicePin>,
    pub index: Vec<IndexEntry>,
}

/// A value in the `[extensions]` table — either an exact version pin
/// (string) or the `false` sentinel that opts a baseline extension out
/// of the project's auto-enable set (CLI.md §3.3 step 4).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ExtensionPin {
    /// `mysqli = false` — disable a baseline extension for this project.
    /// `true` parses successfully but has no semantic meaning today;
    /// only `false` is normative.
    Disabled(bool),
    /// `xdebug = "3.5.1"` — exact-version pin. Constraint shapes are
    /// not accepted (CLI.md §3.2.1).
    Version(String),
}

impl ExtensionPin {
    /// Returns the version pin if one was set. `false` (or any bool)
    /// yields `None`.
    pub fn as_version(&self) -> Option<&str> {
        match self {
            Self::Version(v) => Some(v),
            Self::Disabled(_) => None,
        }
    }

    /// `true` only for the literal `false` sentinel. A bare `true`
    /// pin is also treated as disabled — there's no other meaning
    /// for a boolean here, and rejecting it would just force users
    /// to learn the asymmetry.
    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled(_))
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PhpConfig {
    pub version: Option<String>,
    pub flavor: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ComposerConfig {
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IndexEntry {
    pub host: String,
    pub fingerprint: String,
}

/// A value in the `[services]` table — either a bare version pin
/// (string) or a table with per-service options. See CLI.md §4.2 and
/// SERVICES.md §3.1.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ServicePin {
    /// `redis = "11.4"` or `redis = "*"`. The version is intersected
    /// against the catalog's `version`; `"*"` means "use the catalog
    /// default".
    Version(String),
    /// `[services.mariadb] version = "11.4"; tenant = "myapp"`.
    Detail(ServicePinDetail),
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ServicePinDetail {
    pub version: Option<String>,
    /// Override the default tenant name (composer.json `name`, or cwd
    /// basename if no composer.json). See SERVICES.md §3.1.
    pub tenant: Option<String>,
}

impl ServicePin {
    /// The version pin if one was set. `"*"` is returned as-is; the
    /// resolver translates it to the catalog default at sync time.
    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Version(v) => Some(v),
            Self::Detail(d) => d.version.as_deref(),
        }
    }

    /// The user-provided tenant override, if any.
    pub fn tenant(&self) -> Option<&str> {
        match self {
            Self::Version(_) => None,
            Self::Detail(d) => d.tenant.as_deref(),
        }
    }
}
