//! Real-opensearch test fixture.
//!
//! Same shape as `mariadb_fixture`: download the tarball once into
//! `$HOME/.cache/bougie-test-fixtures/` keyed by sha, then extract
//! into a per-test `BOUGIE_HOME/store/opensearch-2.19.5/` so the
//! supervisor's `binary_path` resolver finds `bin/opensearch` at the
//! catalog-expected path. The opensearch tarball bundles its own
//! Temurin JDK under `install/jdk/`, so we strip the `install/`
//! prefix at extract time the same way bougie's interpreter installer
//! does (see `src/install.rs`).
//!
//! Per-target blob coordinates pulled from
//! `https://index.bougie.tools/versions/r25884606531/...`. Bump URL +
//! sha together when the index publishes a newer tag.

#![allow(dead_code)]

use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Catalog version + tarball name. Must match `daemon::catalog`'s
/// opensearch entry.
pub const OPENSEARCH_TARBALL: &str = "opensearch-2.19.5";

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLOB_URL: &str =
    "https://blobs.bougie.tools/blobs/b6/b676137a1be546ffb9a2045827ee3223ff4bf64b34c3284c14d01e8448ca06c6";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLOB_SHA256: &str = "b676137a1be546ffb9a2045827ee3223ff4bf64b34c3284c14d01e8448ca06c6";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const BLOB_URL: &str =
    "https://blobs.bougie.tools/blobs/cb/cbb115841d51fe247e57a41c8d48ee6549a4432c5658cf551d9e99d093b07ac0";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const BLOB_SHA256: &str = "cbb115841d51fe247e57a41c8d48ee6549a4432c5658cf551d9e99d093b07ac0";

#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
)))]
compile_error!(
    "phase18 opensearch fixture: no opensearch-2.19.5 tarball published for this \
     (os, arch) pair in the bougie index. Add the blob URL + sha256 to \
     tests/common/opensearch_fixture.rs, or set BOUGIE_SKIP_REAL_OPENSEARCH=1 \
     to skip phase18 entirely on this target."
);

/// Materialise `<bougie_home>/store/opensearch-2.19.5/...` from the
/// cached tarball, downloading it on first call.
pub fn install_into(bougie_home: &Path) {
    let blob = ensure_blob_cached();
    let dest = bougie_home.join("store").join(OPENSEARCH_TARBALL);
    if dest.join("bin/opensearch").exists() {
        return;
    }
    fs::create_dir_all(&dest).expect("mkdir opensearch store dir");
    extract_zstd_tar_stripping_install_prefix(&blob, &dest);
    assert!(
        dest.join("bin/opensearch").exists(),
        "expected bin/opensearch at {} after extract",
        dest.display()
    );
    assert!(
        dest.join("jdk/bin/java").exists(),
        "expected bundled bin/java at {}/jdk/bin/java after extract",
        dest.display()
    );
}

fn ensure_blob_cached() -> PathBuf {
    let cache_root = test_cache_root();
    fs::create_dir_all(&cache_root).expect("mkdir test fixture cache");
    let blob_path = cache_root.join(format!("{BLOB_SHA256}.tar.zst"));
    if blob_path.exists() && verify_sha256(&blob_path) {
        return blob_path;
    }

    eprintln!(
        "[opensearch_fixture] downloading {BLOB_URL} -> {}",
        blob_path.display()
    );
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        // 274 MB; CI runners on slow networks have been seen needing
        // a few minutes. Keep the total cap generous.
        .timeout(std::time::Duration::from_mins(10))
        .build()
        .expect("reqwest client");
    let resp = client
        .get(BLOB_URL)
        .send()
        .expect("downloading opensearch blob")
        .error_for_status()
        .expect("opensearch blob HTTP non-2xx");
    let bytes = resp.bytes().expect("reading opensearch blob body");

    let actual = sha256_hex(&bytes);
    assert_eq!(
        actual, BLOB_SHA256,
        "opensearch blob sha256 mismatch: got {actual}, expected {BLOB_SHA256}"
    );
    let tmp = blob_path.with_extension("tmp");
    fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(&bytes))
        .expect("writing opensearch blob to cache");
    fs::rename(&tmp, &blob_path).expect("rename opensearch blob into cache");
    blob_path
}

fn test_cache_root() -> PathBuf {
    if let Ok(dir) = std::env::var("BOUGIE_TEST_FIXTURE_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("bougie-test-fixtures");
    }
    PathBuf::from("/tmp/bougie-test-fixtures")
}

fn verify_sha256(path: &Path) -> bool {
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
    hex_lower(&hasher.finalize()) == BLOB_SHA256
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
    let f = fs::File::open(blob).expect("opening cached opensearch blob");
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
