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

mod collect;
mod emit;
mod installed;
mod lock;
mod scan;
mod vendored;
mod version;

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

/// PSR-noncompliance report for a single class. Composer prints one
/// of these per rejected class when a file's classes all failed the
/// `psr-4` / `psr-0` namespace+path rule: it drops every class in
/// the file and warns. bougie collects them so the CLI can render
/// the same `Class X located in Y does not comply with psr-N
/// autoloading standard. Skipping.` line.
///
/// `relative_path` is already prefixed with `./` to match Composer's
/// `preg_replace('{^getcwd()}', '.', ...)` output. The string is
/// rendered with forward slashes on every platform.
#[derive(Debug, Clone)]
pub struct PsrWarning {
    pub class: String,
    pub relative_path: String,
    /// 0 for PSR-0, 4 for PSR-4 — used as the literal in the
    /// rendered `psr-N` token.
    pub psr_version: u8,
}

/// Summary returned by [`dump_autoload`]. `class_count` matches
/// Composer's `containing N classes` figure: total entries in the
/// emitted classmap (always includes the synthetic
/// `Composer\InstalledVersions` row, mirroring Composer).
#[derive(Debug, Clone)]
pub struct DumpReport {
    pub class_count: usize,
    pub warnings: Vec<PsrWarning>,
}

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
pub fn dump_autoload(req: &DumpRequest<'_>) -> Result<DumpReport, DumpError> {
    let lock = lock::read_lock(req.project_root)?;
    let manifest = lock::read_root_manifest(req.project_root)?;

    let composer_dir = req.project_root.join("vendor").join("composer");
    std::fs::create_dir_all(&composer_dir)?;

    // Composer's resolution order (mirrors getAutoloadFile in
    // AutoloadGenerator.php): explicit setter → `config.autoloader-
    // suffix` from composer.json → existing `vendor/autoload.php`
    // suffix → `composer.lock` content-hash → random hex. We
    // implement the first two and fall through to content-hash. The
    // "scrape existing autoload.php" step matters only for re-dump
    // scenarios; bougie's harness always starts fresh.
    let suffix: String = req
        .autoloader_suffix
        .clone()
        .or_else(|| manifest.config.autoloader_suffix.clone())
        .unwrap_or_else(|| lock.content_hash.clone());

    // APCu prefix: explicit override → Composer's
    // `bin2hex(random_bytes(10))` default. Random is fine here — the
    // value is purely a cache namespace; cross-process collision risk
    // is what the 20 hex chars are sized against.
    let apcu_prefix: Option<String> = if req.apcu_autoloader {
        Some(
            req.apcu_prefix
                .clone()
                .unwrap_or_else(|| random_hex_chars(20)),
        )
    } else {
        None
    };

    let psr4 = collect::psr4(&manifest, &lock, req.no_dev);
    let psr0 = collect::psr0(&manifest, &lock, req.no_dev);
    let files = collect::files(&manifest, &lock, req.no_dev);
    // `--classmap-authoritative` implies `--optimize` (Composer's
    // dump() does `if (classmapAuthoritative) $scanPsrPackages = true`).
    // The flag's other effect — narrowing autoload_real.php's runtime
    // lookup — lives in the static-loader emit, which is Phase 3.
    let optimize = req.optimize || req.classmap_authoritative;
    let classmap_out = collect::classmap(&manifest, &lock, req.no_dev, optimize, req.project_root);
    let classmap = classmap_out.entries;

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
        emit::entry(&suffix).as_bytes(),
    )?;

    write_atomic(
        &composer_dir.join("autoload_real.php"),
        emit::real::emit(
            &suffix,
            !files.is_empty(),
            req.classmap_authoritative,
            apcu_prefix.as_deref(),
        )
        .as_bytes(),
    )?;

    write_atomic(
        &composer_dir.join("autoload_static.php"),
        emit::static_loader::emit(&suffix, &psr4, &psr0, &classmap, &files).as_bytes(),
    )?;

    // Composer copies ClassLoader.php, InstalledVersions.php, and
    // LICENSE verbatim from its own source into vendor/composer/ —
    // we ship pinned copies under crates/bougie-autoloader/vendored/
    // and write them the same way.
    vendored::write_runtime_files(&composer_dir, write_atomic)?;

    // `vendor/composer/installed.{json,php}` mirror Composer's
    // `FilesystemRepository::write` — installed.json re-serializes
    // composer.lock's package metadata, installed.php is the runtime
    // target for `Composer\InstalledVersions::getVersion(...)` etc.
    write_atomic(
        &composer_dir.join("installed.json"),
        installed::emit_installed_json(req.project_root, req.no_dev)?.as_bytes(),
    )?;
    write_atomic(
        &composer_dir.join("installed.php"),
        installed::emit_installed_php(req.project_root, req.no_dev)?.as_bytes(),
    )?;

    let class_count = classmap.len();
    let warnings = classmap_out
        .warnings
        .into_iter()
        .map(|w| PsrWarning {
            class: w.class,
            relative_path: format_relative_path(&w.file, req.project_root),
            psr_version: if w.psr0 { 0 } else { 4 },
        })
        .collect();

    Ok(DumpReport {
        class_count,
        warnings,
    })
}

/// Format an absolute file path the way Composer does in its
/// noncompliance warning: replace the leading project root with `.`
/// (so output starts with `./`) and normalize to forward slashes for
/// cross-platform-consistent display. Falls back to the input path
/// when the prefix doesn't match (canonicalize mismatch, etc.).
fn format_relative_path(file: &Path, project_root: &Path) -> String {
    let canon_root = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.into());
    let rel = file.strip_prefix(&canon_root).unwrap_or(file);
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if rel_str.starts_with("./") || rel_str == "." {
        rel_str
    } else {
        format!("./{rel_str}")
    }
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

