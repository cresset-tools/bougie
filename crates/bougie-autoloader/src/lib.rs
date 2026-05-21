//! Generate Composer-compatible `vendor/composer/autoload_*.php`.
//!
//! Goal per `AUTOLOADER_PLAN.md`: byte-equivalent output to Composer's
//! own `dump-autoload`, pinned to a specific upstream version (2.8.12
//! as of the initial fixture set). Performance-first design: parallel
//! file scan, SIMD byte search in the classmap pipeline, lazy I/O.
//!
//! **Status:** every file `composer dump-autoload` writes under
//! `vendor/` is now emitted byte-equivalent across the fixtures in
//! `tests/fixtures/`: `vendor/autoload.php`,
//! `vendor/composer/autoload_{namespaces,psr4,classmap,files,real,static}.php`,
//! the vendored `ClassLoader.php` / `InstalledVersions.php` / `LICENSE`,
//! and `installed.{json,php}`. Conditional features wired in:
//! `--optimize`, `--classmap-authoritative`, `--no-dev`,
//! `--apcu-autoloader` (with explicit `apcu_prefix` override for
//! tests), and `config.autoloader-suffix` (composer.json override of
//! the content-hash). Still pending: `config.platform-check` →
//! `platform_check.php` (needs a constraint-parsing facility we don't
//! yet have).

mod autoloader;
mod collect;
mod emit;
mod installed;
mod lock;
mod scan;
mod vendored;
mod version;

pub use autoloader::{user_code_roots, AutoloadHeader, Autoloader, HeaderFlags};

/// Internal entry points exposed only so the in-tree
/// `benches/scan.rs` criterion harness can call them. Not a stable
/// API — names and signatures move with the implementation.
#[doc(hidden)]
pub mod bench_api {
    pub fn clean(input: &[u8]) -> Vec<u8> {
        crate::scan::cleaner::clean(input)
    }
    pub fn find_classes(input: &[u8]) -> Vec<String> {
        crate::scan::finder::find_classes(input)
    }
}

/// Internal entry points exposed only for the integration tests under
/// `tests/`. Not a stable API.
#[doc(hidden)]
pub mod test_api {
    pub fn normalize_version(input: &str) -> Result<String, String> {
        crate::version::normalize(input).map_err(|e| e.to_string())
    }
}

use std::path::Path;

/// Pinned upstream Composer version that fixtures + byte-equivalence
/// tests are generated against. Bump in lockstep with regenerating
/// `tests/fixtures/`.
pub const REFERENCE_COMPOSER_VERSION: &str = "2.8.12";

/// Inputs for an autoload dump. Names mirror Composer terminology.
#[derive(Debug, Clone)]
pub struct DumpRequest<'a> {
    /// Root project directory. `composer.json` + `composer.lock` are
    /// read from here; the output is written under `vendor/` here.
    pub project_root: &'a Path,
    /// Whether to use the optimized classmap pipeline (`--optimize`).
    pub optimize: bool,
    /// Whether to emit the classmap-authoritative static loader
    /// (`--classmap-authoritative`). Implies `optimize`.
    pub classmap_authoritative: bool,
    /// Whether to skip dev autoload entries (`--no-dev`).
    pub no_dev: bool,
    /// `--apcu-autoloader` — emits a `setApcuPrefix` call in
    /// `autoload_real.php`. Has no effect unless the PHP runtime has
    /// the APCu extension loaded; the line is a no-op otherwise.
    pub apcu_autoloader: bool,
    /// Explicit APCu prefix override (`--apcu-autoloader-prefix=X`).
    /// When `apcu_autoloader` is true and this is None, Composer
    /// generates a random `bin2hex(random_bytes(10))` prefix; bougie
    /// does the same. Supply an explicit value for byte-equivalence
    /// tests or to stabilize across dumps.
    pub apcu_prefix: Option<String>,
    /// `config.autoloader-suffix` override. When set, replaces both
    /// the value read from `composer.json`'s `config` block and the
    /// `composer.lock` content-hash as the
    /// `ComposerAutoloaderInit<X>` / `ComposerStaticInit<X>` suffix.
    pub autoloader_suffix: Option<String>,
}

#[derive(Debug)]
pub enum DumpError {
    Io(std::io::Error),
    /// `composer.lock` is malformed or has a missing required field.
    Lock(String),
    /// Root `composer.json` is malformed.
    Manifest(String),
}

impl std::fmt::Display for DumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Lock(m) => write!(f, "composer.lock: {m}"),
            Self::Manifest(m) => write!(f, "composer.json: {m}"),
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
/// Thin wrapper around [`Autoloader::bootstrap`] + [`Autoloader::emit`].
/// CLI callers (`bougie composer dump-autoloader`) and the in-process
/// install path (`bougie_composer_resolver::install::orchestrate`)
/// both use this. The long-running server holds an `Autoloader`
/// directly so it can apply incremental edits without re-walking the
/// whole project; see `INCREMENTAL_AUTOLOADER_PLAN.md`.
pub fn dump_autoload(req: &DumpRequest<'_>) -> Result<(), DumpError> {
    let loader = Autoloader::bootstrap(req)?;
    loader.emit()
}

/// Lightweight ASCII-hex randomness for the APCu prefix default.
/// Mirrors PHP's `bin2hex(random_bytes(n/2))`. `n` is the output
/// length in hex chars (so `random_bytes(10)` → 20-char hex prefix).
///
/// Source of entropy: nanos-since-epoch XOR'd with the process ID
/// and the address of a stack local — enough to avoid same-tick
/// collisions on the same host. Composer itself uses a CSPRNG; the
/// prefix's job is purely to namespace the APCu cache so two
/// unrelated projects on the same SAPI don't share entries. For
/// byte-equivalence tests, callers should pass an explicit
/// `apcu_prefix` (no fallback to randomness then).
fn random_hex_chars(n: usize) -> String {
    use std::fmt::Write as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    let local = 0u8;
    let mut state: u128 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ u128::from(std::process::id())
        ^ (std::ptr::addr_of!(local) as u128);

    let mut out = String::with_capacity(n);
    while out.len() < n {
        // xorshift64-style step on each 64-bit half of the 128-bit
        // state. We don't need crypto-grade output, just uncorrelated
        // bytes for a cache-namespace tag.
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        for byte in state.to_le_bytes() {
            if out.len() >= n {
                break;
            }
            let _ = write!(out, "{byte:02x}");
        }
    }
    out.truncate(n);
    out
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Rename-based atomicity: write to <path>.tmp then rename.
    // Cheap insurance against partial writes from interrupted runs.
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

