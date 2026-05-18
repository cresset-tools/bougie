//! Generate Composer-compatible `vendor/composer/autoload_*.php`.
//!
//! Goal per `AUTOLOADER_PLAN.md`: byte-equivalent output to Composer's
//! own `dump-autoload`, pinned to a specific upstream version (2.8.12
//! as of the initial fixture set). Performance-first design: parallel
//! file scan, SIMD byte search in the classmap pipeline, lazy I/O.
//!
//! **Status:** crate skeleton + Phase 0 fixture harness. The actual
//! emitters (`autoload_namespaces.php`, `autoload_psr4.php`,
//! `autoload_files.php`, `autoload_classmap.php`, `autoload_real.php`,
//! `autoload_static.php`, `autoload.php`, `installed.json`,
//! `installed.php`, `InstalledVersions.php`, `ClassLoader.php`,
//! `LICENSE`, `platform_check.php`) land in subsequent PRs, one phase
//! at a time, gated by `tests/byte_equivalence.rs`.

use std::path::Path;

/// Pinned upstream Composer version that fixtures + byte-equivalence
/// tests are generated against. Bump in lockstep with regenerating
/// `tests/fixtures/`.
pub const REFERENCE_COMPOSER_VERSION: &str = "2.8.12";

/// Inputs for an autoload dump. Names mirror Composer terminology.
#[derive(Debug, Clone)]
pub struct DumpRequest<'a> {
    /// Root project directory. `composer.json` is read from here; the
    /// output `vendor/` is written under it.
    pub project_root: &'a Path,
    /// Whether to use the optimized classmap pipeline (`--optimize`).
    pub optimize: bool,
    /// Whether to emit the classmap-authoritative static loader
    /// (`--classmap-authoritative`). Implies `optimize`.
    pub classmap_authoritative: bool,
    /// Whether to skip dev autoload entries (`--no-dev`).
    pub no_dev: bool,
}

#[derive(Debug)]
pub enum DumpError {
    /// Reserved for the pre-Phase-1 period. Tests should treat this
    /// as "not yet implemented" rather than "input was invalid".
    Unimplemented,
    Io(std::io::Error),
}

impl std::fmt::Display for DumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unimplemented => write!(f, "autoload dump not yet implemented"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for DumpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DumpError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Generate `vendor/composer/autoload_*.php` for the given project.
///
/// **Not yet implemented.** Phase 1 of `AUTOLOADER_PLAN.md` provides
/// the PSR-4 / PSR-0 / files emitters; classmap follows in Phase 2;
/// `autoload_static.php` in Phase 3.
pub fn dump_autoload(_req: &DumpRequest<'_>) -> Result<(), DumpError> {
    Err(DumpError::Unimplemented)
}
