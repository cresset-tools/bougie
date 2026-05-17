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

mod emit;
mod lock;

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
/// Phase 1 emits: `vendor/autoload.php` (entry point),
/// `vendor/composer/autoload_namespaces.php` (PSR-0),
/// `vendor/composer/autoload_psr4.php`,
/// `vendor/composer/autoload_files.php` (only if any package or root
/// declares `files`). Phase 2 adds `autoload_classmap.php`; Phase 3
/// adds `autoload_real.php` + `autoload_static.php` and the vendored
/// runtime files.
pub fn dump_autoload(req: &DumpRequest<'_>) -> Result<(), DumpError> {
    let lock = lock::read_lock(req.project_root)?;
    let manifest = lock::read_root_manifest(req.project_root)?;

    let composer_dir = req.project_root.join("vendor").join("composer");
    std::fs::create_dir_all(&composer_dir)?;

    let psr4 = collect::psr4(&manifest, &lock, req.no_dev);
    let psr0 = collect::psr0(&manifest, &lock, req.no_dev);
    let files = collect::files(&manifest, &lock, req.no_dev);

    write_atomic(
        &composer_dir.join("autoload_psr4.php"),
        emit::psr4(&psr4).as_bytes(),
    )?;
    write_atomic(
        &composer_dir.join("autoload_namespaces.php"),
        emit::psr0(&psr0).as_bytes(),
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

mod collect {
    use super::lock::{LockFile, RootManifest};

    /// One PSR-4 or PSR-0 prefix and its install-path-prefixed dirs.
    /// Order: per-package autoload entries in lockfile order, then
    /// root entries last (Composer's own order — root overrides come
    /// after package entries in the array; PHP arrays preserve insertion).
    pub(super) struct Entry {
        pub prefix: String,
        pub paths: Vec<String>,
    }

    pub(super) fn psr4(root: &RootManifest, lock: &LockFile, no_dev: bool) -> Vec<Entry> {
        let mut out = vec![];
        for pkg in lock.iter_packages(no_dev) {
            let install_path = format!("vendor/{}", pkg.name);
            for (prefix, dirs) in &pkg.autoload.psr4 {
                let paths = dirs
                    .iter()
                    .map(|d| format!("$vendorDir . '/{}'", join_rel(&install_path, d)))
                    .collect();
                out.push(Entry {
                    prefix: prefix.clone(),
                    paths,
                });
            }
        }
        // Root PSR-4 last.
        for (prefix, dirs) in &root.autoload.psr4 {
            let paths = dirs
                .iter()
                .map(|d| format!("$baseDir . '/{}'", strip_leading_slash(d)))
                .collect();
            out.push(Entry {
                prefix: prefix.clone(),
                paths,
            });
        }
        out
    }

    pub(super) fn psr0(root: &RootManifest, lock: &LockFile, no_dev: bool) -> Vec<Entry> {
        let mut out = vec![];
        for pkg in lock.iter_packages(no_dev) {
            let install_path = format!("vendor/{}", pkg.name);
            for (prefix, dirs) in &pkg.autoload.psr0 {
                let paths = dirs
                    .iter()
                    .map(|d| format!("$vendorDir . '/{}'", join_rel(&install_path, d)))
                    .collect();
                out.push(Entry {
                    prefix: prefix.clone(),
                    paths,
                });
            }
        }
        for (prefix, dirs) in &root.autoload.psr0 {
            let paths = dirs
                .iter()
                .map(|d| format!("$baseDir . '/{}'", strip_leading_slash(d)))
                .collect();
            out.push(Entry {
                prefix: prefix.clone(),
                paths,
            });
        }
        out
    }

    pub(super) struct FileEntry {
        pub identifier: String,
        pub path_expr: String,
    }

    pub(super) fn files(root: &RootManifest, lock: &LockFile, no_dev: bool) -> Vec<FileEntry> {
        let mut out = vec![];
        for pkg in lock.iter_packages(no_dev) {
            let install_path = format!("vendor/{}", pkg.name);
            for f in &pkg.autoload.files {
                out.push(FileEntry {
                    identifier: md5_hex(&format!("{}:{}", pkg.name, f)),
                    path_expr: format!("$vendorDir . '/{}'", join_rel(&install_path, f)),
                });
            }
        }
        for f in &root.autoload.files {
            out.push(FileEntry {
                identifier: md5_hex(&format!("__root__:{}", f)),
                path_expr: format!("$baseDir . '/{}'", strip_leading_slash(f)),
            });
        }
        out
    }

    /// Composer normalizes `psr-4`/`psr-0` paths by stripping leading
    /// `./` and trailing `/`. `join_rel` builds the literal that
    /// appears in PHP source: `<install_path>/<dir>` minus the
    /// trailing slash (Composer omits it). Empty `dir` collapses to
    /// just the install path.
    fn join_rel(install_path: &str, dir: &str) -> String {
        let trimmed = strip_leading_slash(dir).trim_end_matches('/');
        if trimmed.is_empty() {
            install_path.strip_prefix("vendor/").map_or_else(|| install_path.to_string(), |s| s.to_string())
        } else {
            // install_path is "vendor/<name>" — drop the "vendor/" prefix
            // because the path expr is already `$vendorDir . '/...`.
            let pkg_part = install_path.strip_prefix("vendor/").unwrap_or(install_path);
            format!("{pkg_part}/{trimmed}")
        }
    }

    fn strip_leading_slash(s: &str) -> &str {
        s.strip_prefix('/').unwrap_or(s)
    }

    /// Minimal MD5 — md5 is needed for file identifiers and we keep
    /// the crate dependency-light. Implementation: RFC 1321,
    /// translated to Rust. Fine for the small inputs we hash
    /// (package_name + ':' + path).
    fn md5_hex(input: &str) -> String {
        let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
        let mut bytes = input.as_bytes().to_vec();
        let bit_len = (bytes.len() as u64).wrapping_mul(8);
        bytes.push(0x80);
        while bytes.len() % 64 != 56 {
            bytes.push(0);
        }
        bytes.extend_from_slice(&bit_len.to_le_bytes());

        for chunk in bytes.chunks(64) {
            let mut m = [0u32; 16];
            for i in 0..16 {
                m[i] = u32::from_le_bytes([
                    chunk[i * 4],
                    chunk[i * 4 + 1],
                    chunk[i * 4 + 2],
                    chunk[i * 4 + 3],
                ]);
            }
            let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);

            // Standard MD5 rounds. Indexed table-driven.
            for i in 0..64 {
                let (f, g) = match i {
                    0..=15 => (((b & c) | (!b & d)), i),
                    16..=31 => (((d & b) | (!d & c)), (5 * i + 1) % 16),
                    32..=47 => ((b ^ c ^ d), (3 * i + 5) % 16),
                    _ => ((c ^ (b | !d)), (7 * i) % 16),
                };
                let temp = d;
                d = c;
                c = b;
                b = b.wrapping_add(
                    a.wrapping_add(f)
                        .wrapping_add(MD5_K[i])
                        .wrapping_add(m[g])
                        .rotate_left(MD5_S[i]),
                );
                a = temp;
            }

            state[0] = state[0].wrapping_add(a);
            state[1] = state[1].wrapping_add(b);
            state[2] = state[2].wrapping_add(c);
            state[3] = state[3].wrapping_add(d);
        }

        let mut out = String::with_capacity(32);
        for word in state {
            for byte in word.to_le_bytes() {
                use std::fmt::Write;
                let _ = write!(out, "{byte:02x}");
            }
        }
        out
    }

    const MD5_S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    const MD5_K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn md5_known_vector() {
            assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
            assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
            // The exact identifier composer 2.8 produces for the
            // files-single fixture, cross-checked via `php -r 'echo md5(...)'`.
            assert_eq!(
                md5_hex("acme/helpers:functions.php"),
                "15a74e8c7f50af51efa9794609612b23"
            );
        }
    }
}
