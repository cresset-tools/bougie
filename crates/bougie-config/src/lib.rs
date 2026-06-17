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
    pub extensions: BTreeMap<String, ExtensionPin>,
    pub services: BTreeMap<String, ServicePin>,
    pub index: Vec<IndexEntry>,
    pub server: ServerConfig,
    pub scripts: ScriptsConfig,
    pub patches: PatchesConfig,
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
    /// System-vs-managed PHP preference (uv's model). `Some(true)` ⇒
    /// only use a bougie-managed PHP; `Some(false)` ⇒ only use a system
    /// PHP; `None` ⇒ default (prefer installed managed, then system,
    /// then download). Overridden by `--managed-php`/`--no-managed-php`.
    pub managed: Option<bool>,
    /// `Some(false)` ⇒ never download a managed PHP (use an installed
    /// managed one or a system PHP). Overridden by `--no-php-downloads`.
    pub downloads: Option<bool>,
}

/// Opt-in execution of root `composer.json` scripts (off by default).
/// Composer only runs scripts from the *root* package, so they're the
/// project author's own commands — but a freshly-cloned untrusted repo's
/// `post-install-cmd` must not auto-run, hence opt-in. Lives under
/// `[scripts]` in `bougie.toml` or `extra.bougie.scripts` in
/// `composer.json`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ScriptsConfig {
    /// `run = true` enables running root scripts during the install
    /// lifecycle. `None` means unset (treated as `false`); a CLI
    /// `--scripts`/`--no-scripts` flag overrides this.
    pub run: Option<bool>,
}

impl ScriptsConfig {
    /// Whether script execution is enabled (unset → false).
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.run.unwrap_or(false)
    }
}

/// Native patch application (the `cweagans/composer-patches` reimplementation).
/// Mirrors cweagans' superset of config; lives under `[patches]` in
/// `bougie.toml` or `extra.bougie.patches` in `composer.json`. The Composer
/// `extra.patches` / `extra.composer-patches.*` keys are read separately from
/// `composer.json` `extra` by `bougie-patches` (they're Composer-namespaced,
/// not bougie config).
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PatchesConfig {
    /// `enable-patching` superset. When unset, patching runs whenever the
    /// root declares any patch (inline, patches-file, or a `patches/` dir);
    /// `Some(false)` forces it off, `Some(true)` forces it on. A CLI
    /// `--patches`/`--no-patches` flag overrides this.
    pub enable: Option<bool>,
    /// `patches/` directory override (default `"patches"`).
    pub dir: Option<String>,
    /// `composer-exit-on-patch-failure` superset: abort the install on the
    /// first failed patch instead of skip-and-warn.
    pub exit_on_failure: Option<bool>,
    /// ALSO emit the v2-shaped human `patches.lock.json` serialization; the
    /// fingerprint store itself is always written regardless.
    pub write_lock: Option<bool>,
    /// `COMPOSER_PATCHES_SKIP_REPORTING` superset: suppress `PATCHES.txt`.
    pub skip_report: Option<bool>,
}

/// Project-level overrides for the supervised `bougie server` host
/// block. Today the only knob is `root`, used to escape the
/// `pub` → `public` → error auto-detection. Lives under
/// `[server]` in `bougie.toml` or `extra.bougie.server` in
/// `composer.json`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Project-relative docroot. When unset, the provisioner picks
    /// `pub` if present, else `public`, else errors with a hint to
    /// set this field.
    pub root: Option<String>,
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
