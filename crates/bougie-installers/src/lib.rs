//! Native reimplementation of the two *declarative* Composer install
//! plugins that Magento / Mage-OS projects depend on. Bougie never runs
//! Composer plugins or install scripts (see the workspace `CLAUDE.md`
//! invariant), so it reproduces their on-disk effect instead. Both
//! plugins are fully declarative — every input lives in `composer.json`
//! — which is what makes a native port possible.
//!
//! - [`deploy`] — `magento/magento-composer-installer`. A
//!   `magento2-component` package (canonically `magento/magento2-base`)
//!   declares an `extra.map` of `[source, dest]` pairs that get copied
//!   into the project root (this is how `index.php`, `pub/`, the
//!   `app/etc/*` skeleton land) plus an `extra.chmod` list of permission
//!   masks. The plugin also generates `app/etc/vendor_path.php`, which
//!   Magento's bootstrap reads to locate `vendor/`.
//!
//! - [`paths`] — `composer/installers`. A generic `type` → install-path
//!   router (e.g. `magento-theme` → `app/design/frontend/{$name}/`),
//!   with root `extra.installer-paths` overrides. Pure relocation; no
//!   copying.

pub mod deploy;
pub mod laravel;
pub mod paths;

pub use deploy::{ChmodEntry, DeployPlan, DeployStats, VENDOR_PATH_PHP, apply_deploy, plan_deploy};
pub use laravel::{
    PACKAGES_CACHE, STALE_CACHES, blocking_post_autoload_dump, build_package_manifest,
    render_packages_php,
};
pub use paths::{
    InstallerPaths, install_path, install_path_relative_to_repo, unsupported_framework,
};
