//! Generate Composer-compatible `vendor/composer/autoload_*.php`.
//!
//! Goal per `AUTOLOADER_PLAN.md`: byte-equivalent output to Composer's
//! own `dump-autoload`, pinned to a specific upstream version (2.8.12
//! as of the initial fixture set). Performance-first design: parallel
//! file scan, SIMD byte search in the classmap pipeline, lazy I/O.
//!
//! **Status:** Phase 1 — PSR-4, PSR-0, files emitters land. Classmap
//! scanning (Phase 2), autoload_real.php + autoload_static.php
//! (Phase 3), vendored ClassLoader / InstalledVersions / LICENSE
//! (deferred), installed.json / installed.php regeneration (deferred)
//! arrive in subsequent PRs. The byte-equivalence harness in
//! `tests/byte_equivalence.rs` checks only what each phase ships.

mod collect;
mod emit;
mod lock;
mod scan;
mod vendored;

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
/// Phase 1+2 emits: `vendor/autoload.php`,
/// `vendor/composer/autoload_namespaces.php` (PSR-0),
/// `vendor/composer/autoload_psr4.php`,
/// `vendor/composer/autoload_classmap.php` (always — at minimum the
/// `Composer\InstalledVersions` stub), and
/// `vendor/composer/autoload_files.php` (only if any package or root
/// declares `files`). Phase 3 adds `autoload_real.php` +
/// `autoload_static.php` and the vendored runtime files;
/// `--optimize` / `--classmap-authoritative` / `exclude-from-classmap`
/// are still pending wiring.
pub fn dump_autoload(req: &DumpRequest<'_>) -> Result<(), DumpError> {
    let lock = lock::read_lock(req.project_root)?;
    let manifest = lock::read_root_manifest(req.project_root)?;

    let composer_dir = req.project_root.join("vendor").join("composer");
    std::fs::create_dir_all(&composer_dir)?;

    let psr4 = collect::psr4(&manifest, &lock, req.no_dev);
    let psr0 = collect::psr0(&manifest, &lock, req.no_dev);
    let files = collect::files(&manifest, &lock, req.no_dev);
    // `--classmap-authoritative` implies `--optimize` (Composer's
    // dump() does `if (classmapAuthoritative) $scanPsrPackages = true`).
    // The flag's other effect — narrowing autoload_real.php's runtime
    // lookup — lives in the static-loader emit, which is Phase 3.
    let optimize = req.optimize || req.classmap_authoritative;
    let classmap = collect::classmap(&manifest, &lock, req.no_dev, optimize, req.project_root);

    write_atomic(
        &composer_dir.join("autoload_psr4.php"),
        emit::psr4(&psr4).as_bytes(),
    )?;
    write_atomic(
        &composer_dir.join("autoload_namespaces.php"),
        emit::psr0(&psr0).as_bytes(),
    )?;
    write_atomic(
        &composer_dir.join("autoload_classmap.php"),
        emit::classmap(&classmap).as_bytes(),
    )?;
    if !files.is_empty() {
        write_atomic(
            &composer_dir.join("autoload_files.php"),
            emit::files(&files).as_bytes(),
        )?;
    }

    write_atomic(
        &req.project_root.join("vendor").join("autoload.php"),
        emit::entry(&lock.content_hash).as_bytes(),
    )?;

    write_atomic(
        &composer_dir.join("autoload_real.php"),
        emit::real::emit(
            &lock.content_hash,
            !files.is_empty(),
            req.classmap_authoritative,
        )
        .as_bytes(),
    )?;

    write_atomic(
        &composer_dir.join("autoload_static.php"),
        emit::static_loader::emit(&lock.content_hash, &psr4, &psr0, &classmap, &files).as_bytes(),
    )?;

    // Composer copies ClassLoader.php, InstalledVersions.php, and
    // LICENSE verbatim from its own source into vendor/composer/ —
    // we ship pinned copies under crates/bougie-autoloader/vendored/
    // and write them the same way.
    vendored::write_runtime_files(&composer_dir, write_atomic)?;

    Ok(())
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

