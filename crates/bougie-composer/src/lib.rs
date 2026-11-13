//! Composer (the PHP package manager) data model.
//!
//! bougie does not bundle or execute the Composer phar — it
//! reimplements the Composer surface natively. The `composer.json` /
//! `composer.lock` model ([`lockfile`]) and the Packagist v2 metadata
//! wire format ([`metadata`]) come from the shared `composer-manifest`
//! crate, re-exported here under their old paths. What stays local is
//! bougie's own: the inline metadata of single-file PHP scripts
//! ([`inline`]).

pub mod inline;

pub use composer_manifest::{lockfile, metadata};

/// Re-export of the [`composer_php_json`] crate (the byte-exact PHP
/// `json_encode`, extracted to the shared `composer-rs` workspace). Kept
/// under `bougie_composer::php_json` for backwards compatibility with the
/// pre-extraction module path; new consumers should depend on
/// `composer-php-json` directly.
pub use composer_php_json as php_json;
