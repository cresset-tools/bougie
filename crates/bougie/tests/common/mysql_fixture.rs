//! Real-mysql test fixture.
//!
//! Downloads a mysql tarball from the bougie index *once* per version,
//! caches it under `$HOME/.cache/bougie-test-fixtures/`, and extracts
//! into a per-test `BOUGIE_HOME/store/mysql-<version>/` so the supervisor
//! finds the real `bin/mysqld` at the path the catalog expects.
//!
//! Unlike the mariadb fixture this stages *two* versions (8.4 + 8.0) so
//! the multi-instance suite (`phase25`) can run both at once. The blob
//! URL is content-addressed (`blobs/<first2>/<sha>`), so a version's
//! coordinates are fully determined by its sha256.
//!
//! sha256s are hard-coded per (target, version); if the index
//! re-publishes under a new content hash, bump them here.

#![allow(dead_code)]

use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Catalog versions. Must match `daemon::catalog`'s mysql entry exactly
/// so the supervisor's `binary_path` resolver lands on the extracted dir.
pub const MYSQL_8_4: &str = "8.4.10";
pub const MYSQL_8_0: &str = "8.0.46";

// Per-target blob sha256s. The CI matrix is `{ubuntu-latest,
// macos-latest}`, so we cover `x86_64-unknown-linux-gnu` and
// `aarch64-apple-darwin`. Pulled from the `tool/mysql` manifests at
// index publish `r28937971937`; bump when the index republishes.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SHA_8_4: &str = "ba3ea3c818c57945ccb45bdf27f940d384fc169f8f0980e2ad297c8e23720f44";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SHA_8_0: &str = "2fd39ec3be69d6f07dc2d3933263d22f94454ccf1d11617ca128610950f47610";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const SHA_8_4: &str = "00f43cf3fe7fa69fc8f32540c5af3e9e7ff595f6c4c6fcc76aa6f6b728f7eb25";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const SHA_8_0: &str = "e72fb26bec410489e705e07500250b78ea78b4a9fcf81e03b558deed17b1f559";

// Compile-fail guard for targets the bougie index doesn't publish mysql
// for. Add the shas above (or set BOUGIE_SKIP_REAL_MYSQL=1 to skip).
#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
)))]
compile_error!(
    "phase25 mysql fixture: no mysql tarball published for this (os, arch) \
     pair in the bougie index. Add the blob sha256s to \
     tests/common/mysql_fixture.rs, or set BOUGIE_SKIP_REAL_MYSQL=1 to \
     skip phase25 entirely on this target."
);

fn sha_for(version: &str) -> &'static str {
    match version {
        MYSQL_8_4 => SHA_8_4,
        MYSQL_8_0 => SHA_8_0,
        other => panic!("mysql_fixture: no blob coordinates for version `{other}`"),
    }
}

/// Materialise `<bougie_home>/store/mysql-<version>/...` from the cached
/// tarball, downloading it on first call.
pub fn install_into(bougie_home: &Path, version: &str) {
    let sha = sha_for(version);
    let dest = bougie_home.join("store").join(format!("mysql-{version}"));
    if dest.join("bin/mysqld").exists() {
        return;
    }
    let blob = ensure_blob_cached(sha);
    fs::create_dir_all(&dest).expect("mkdir mysql store dir");
    extract_zstd_tar_stripping_install_prefix(&blob, &dest);
    assert!(
        dest.join("bin/mysqld").exists(),
        "expected bin/mysqld at {} after extracting the tarball",
        dest.display()
    );
    assert!(
        dest.join("bin/mysql").exists(),
        "expected bin/mysql client at {} after extracting the tarball",
        dest.display()
    );
}

/// Return a path to the cached tarball for `sha`, downloading it on first
/// call. Cached at `$BOUGIE_TEST_FIXTURE_DIR/<sha>.tar.zst` (env
/// override), or `$HOME/.cache/bougie-test-fixtures/<sha>.tar.zst`.
fn ensure_blob_cached(sha: &str) -> PathBuf {
    let cache_root = test_cache_root();
    fs::create_dir_all(&cache_root).expect("mkdir test fixture cache");
    let blob_path = cache_root.join(format!("{sha}.tar.zst"));
    if blob_path.exists() && verify_sha256(&blob_path, sha) {
        return blob_path;
    }

    let url = format!("https://blobs.bougie.tools/blobs/{}/{sha}", &sha[..2]);
    eprintln!("[mysql_fixture] downloading {url} -> {}", blob_path.display());
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_mins(5))
        .build()
        .expect("reqwest client");
    let resp = client
        .get(&url)
        .send()
        .expect("downloading mysql blob")
        .error_for_status()
        .expect("mysql blob HTTP non-2xx");
    let bytes = resp.bytes().expect("reading mysql blob body");

    let actual = sha256_hex(&bytes);
    assert_eq!(actual, sha, "mysql blob sha256 mismatch: got {actual}, expected {sha}");
    let tmp = blob_path.with_extension("tmp");
    fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(&bytes))
        .expect("writing mysql blob to cache");
    fs::rename(&tmp, &blob_path).expect("rename mysql blob into cache");
    blob_path
}

fn test_cache_root() -> PathBuf {
    if let Ok(dir) = std::env::var("BOUGIE_TEST_FIXTURE_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".cache").join("bougie-test-fixtures");
    }
    PathBuf::from("/tmp/bougie-test-fixtures")
}

fn verify_sha256(path: &Path, expected: &str) -> bool {
    let Ok(mut f) = fs::File::open(path) else { return false };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    hex_lower(&hasher.finalize()) == expected
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex_lower(&h.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn extract_zstd_tar_stripping_install_prefix(blob: &Path, dest: &Path) {
    let f = fs::File::open(blob).expect("opening cached mysql blob");
    let decoder = zstd::stream::read::Decoder::new(f).expect("zstd decoder");
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);
    for entry in archive.entries().expect("iterating tar entries") {
        let mut entry = entry.expect("reading tar entry");
        let path = entry.path().expect("entry path").into_owned();
        let mut comps = path.components();
        let first = comps.next();
        if first.is_some_and(|c| c.as_os_str() == "install") {
            let rest: PathBuf = comps.collect();
            if rest.as_os_str().is_empty() {
                continue;
            }
            entry.unpack(dest.join(rest)).expect("unpacking entry");
        }
    }
}
