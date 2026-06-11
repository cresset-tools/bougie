//! Composer (the PHP package manager) data model.
//!
//! bougie does not bundle or execute the Composer phar — it
//! reimplements the Composer surface natively. This crate holds the
//! Composer-format data models the native implementation reads and
//! writes: `composer.json` / `composer.lock` ([`lockfile`]) and the
//! Packagist v2 metadata wire format ([`metadata`]).

pub mod lockfile;
pub mod metadata;

/// Re-export of the [`bougie_php_json`] crate. Kept under
/// `bougie_composer::php_json` for backwards compatibility with the
/// pre-extraction module path; new consumers should depend on
/// `bougie-php-json` directly.
pub use bougie_php_json as php_json;
