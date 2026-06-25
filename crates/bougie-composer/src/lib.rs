//! Composer (the PHP package manager) data model.
//!
//! bougie does not bundle or execute the Composer phar — it
//! reimplements the Composer surface natively. This crate holds the
//! Composer-format data models the native implementation reads and
//! writes: `composer.json` / `composer.lock` ([`lockfile`]) and the
//! Packagist v2 metadata wire format ([`metadata`]).

pub mod lockfile;
pub mod metadata;

/// Re-export of the [`composer_php_json`] crate (the byte-exact PHP
/// `json_encode`, extracted to the shared `composer-rs` workspace). Kept
/// under `bougie_composer::php_json` for backwards compatibility with the
/// pre-extraction module path; new consumers should depend on
/// `composer-php-json` directly.
pub use composer_php_json as php_json;
