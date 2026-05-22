//! Real-mariadb test fixture.
//!
//! Downloads the mariadb-11.4.4 tarball from the bougie index *once*,
//! caches it under `$HOME/.cache/bougie-test-fixtures/`, and extracts
//! into a per-test `BOUGIE_HOME/store/mariadb-11.4.4/` so the
//! supervisor finds the real `bin/mariadbd` at the path the catalog
//! expects.
//!
//! The tarball ships its tree under `install/`, matching every other
//! cresset-tools tarball; we strip the prefix at extraction time the
//! same way `src/install.rs` does (see `strip_prefix: "install"`).
//!
//! sha256 is hard-coded against the URL — if the index re-publishes
//! under a new content hash, bump both together.

#![allow(dead_code)]

use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Catalog version + tarball name. Must match `daemon::catalog`'s
/// mariadb entry exactly so the supervisor's `binary_path` resolver
/// lands on the extracted directory.
pub const MARIADB_TARBALL: &str = "mariadb-11.4.4";

// Per-target blob coordinates. Pulled from
// `https://index.bougie.tools/versions/r25813121006/targets/<TRIPLE>/sections/tool/mariadb.json`
// — bump both URL and sha together when the index publishes a new tag.
// The CI matrix is `{ubuntu-latest, macos-latest}` so we only need
// `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin`. If a third
// runner triple gets added (musl, x86_64-apple-darwin), thread it
// through here.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLOB_URL: &str =
    "https://blobs.bougie.tools/blobs/0b/0b46049fea5e057fc23d639225623fb36a6a7d52969d351823d883f409e4bb1f";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLOB_SHA256: &str = "0b46049fea5e057fc23d639225623fb36a6a7d52969d351823d883f409e4bb1f";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const BLOB_URL: &str =
    "https://blobs.bougie.tools/blobs/98/98d449051d2e1e155c037e1c03c0aaa34693c5927c0601156eae64e7ead0f1f0";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const BLOB_SHA256: &str = "98d449051d2e1e155c037e1c03c0aaa34693c5927c0601156eae64e7ead0f1f0";

// Compile-fail guard for targets the bougie index doesn't yet publish.
#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
)))]
compile_error!(
    "phase17 mariadb fixture: no mariadb-11.4.4 tarball published for this \
     (os, arch) pair in the bougie index. Add the blob URL + sha256 to \
     tests/common/mariadb_fixture.rs, or set BOUGIE_SKIP_REAL_MARIADB=1 \
     to skip phase17 entirely on this target."
);

/// Materialise `<bougie_home>/store/mariadb-11.4.4/...` from the cached
/// tarball, downloading it on first call.
pub fn install_into(bougie_home: &Path) {
    let blob = ensure_blob_cached();
    let dest = bougie_home.join("store").join(MARIADB_TARBALL);
    if dest.join("bin/mariadbd").exists() {
        return;
    }
    fs::create_dir_all(&dest).expect("mkdir mariadb store dir");
    extract_zstd_tar_stripping_install_prefix(&blob, &dest);
    assert!(
        dest.join("bin/mariadbd").exists(),
        "expected bin/mariadbd at {} after extracting the tarball",
        dest.display()
    );
    assert!(
        dest.join("bin/mariadb-install-db").exists(),
        "expected bin/mariadb-install-db at {} after extracting the tarball",
        dest.display()
    );
}

/// Return a path to the cached tarball, downloading it on first call.
/// Cached at `$BOUGIE_TEST_FIXTURE_DIR/<sha>.tar.zst` (env override),
/// or `$HOME/.cache/bougie-test-fixtures/<sha>.tar.zst` by default.
fn ensure_blob_cached() -> PathBuf {
    let cache_root = test_cache_root();
    fs::create_dir_all(&cache_root).expect("mkdir test fixture cache");
    let blob_path = cache_root.join(format!("{BLOB_SHA256}.tar.zst"));
    if blob_path.exists() && verify_sha256(&blob_path) {
        return blob_path;
    }

    eprintln!("[mariadb_fixture] downloading {BLOB_URL} -> {}", blob_path.display());
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_mins(5))
        .build()
        .expect("reqwest client");
    let resp = client
        .get(BLOB_URL)
        .send()
        .expect("downloading mariadb blob")
        .error_for_status()
        .expect("mariadb blob HTTP non-2xx");
    let bytes = resp.bytes().expect("reading mariadb blob body");

    // Verify before persisting so a corrupt download never poisons
    // the cache for later test runs.
    let actual = sha256_hex(&bytes);
    assert_eq!(
        actual, BLOB_SHA256,
        "mariadb blob sha256 mismatch: got {actual}, expected {BLOB_SHA256}"
    );
    let tmp = blob_path.with_extension("tmp");
    fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(&bytes))
        .expect("writing mariadb blob to cache");
    fs::rename(&tmp, &blob_path).expect("rename mariadb blob into cache");
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
    let f = fs::File::open(blob).expect("opening cached mariadb blob");
    let decoder = zstd::stream::read::Decoder::new(f).expect("zstd decoder");
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);
    for entry in archive.entries().expect("iterating tar entries") {
        let mut entry = entry.expect("reading tar entry");
        let path = entry.path().expect("entry path").into_owned();
        // Strip the leading `install/` segment that every cresset
        // tarball ships under; entries without that prefix (rare) are
        // dropped to keep the store layout clean.
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
