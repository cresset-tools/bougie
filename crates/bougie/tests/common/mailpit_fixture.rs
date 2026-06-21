//! Real-mailpit test fixture.
//!
//! Downloads the upstream Mailpit release tarball from GitHub once,
//! caches it under `$HOME/.cache/bougie-test-fixtures/`, and extracts
//! the single `mailpit` binary into a per-test
//! `BOUGIE_HOME/store/mailpit-1.30.2/bin/mailpit`.
//!
//! Unlike the other service fixtures (rabbitmq/opensearch/mariadb),
//! which pull `.tar.zst` blobs laid out under `install/` from the
//! bougie index, Mailpit is a third-party single static Go binary
//! published as a plain `.tar.gz` on GitHub (the archive holds
//! `mailpit`, `LICENSE`, `README.md` at the root). We place the binary
//! at `bin/mailpit` so it matches the catalog entry's `binary` path —
//! the same layout the bougie index will produce once it republishes
//! Mailpit through `install/bin/mailpit`.

#![allow(dead_code)]

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Catalog version + tarball name. Must match `daemon::catalog`'s
/// mailpit entry exactly so the supervisor's binary resolver lands on
/// the extracted directory.
pub const MAILPIT_TARBALL: &str = "mailpit-1.30.2";

// Per-target asset coordinates from the upstream GitHub release. There
// is no checksums asset published, so the sha256 is computed locally
// from the downloaded tarball — bump both URL and sha together when the
// catalog pins a newer Mailpit.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET_URL: &str =
    "https://github.com/axllent/mailpit/releases/download/v1.30.2/mailpit-linux-amd64.tar.gz";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET_SHA256: &str = "63b113aa9748adf7091b649ebe02693f99a459000cbe415faa6679f4b39f82cf";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const ASSET_URL: &str =
    "https://github.com/axllent/mailpit/releases/download/v1.30.2/mailpit-darwin-arm64.tar.gz";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const ASSET_SHA256: &str = "05b92a4b804c34b0f6e665a482a1141be64256f500ecf23a204c2084a27a248b";

#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
)))]
compile_error!(
    "phase22 mailpit fixture: no mailpit-1.30.2 asset coordinates for this \
     (os, arch) pair. Add the GitHub asset URL + sha256 to \
     tests/common/mailpit_fixture.rs."
);

pub fn install_into(bougie_home: &Path) {
    let blob = ensure_blob_cached();
    let dest = bougie_home.join("store").join(MAILPIT_TARBALL);
    let bin = dest.join("bin").join("mailpit");
    if bin.exists() {
        return;
    }
    fs::create_dir_all(bin.parent().unwrap()).expect("mkdir mailpit store bin dir");
    extract_mailpit_binary(&blob, &bin);
    assert!(
        bin.exists(),
        "expected bin/mailpit at {} after extracting the tarball",
        bin.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();
    }
}

fn ensure_blob_cached() -> PathBuf {
    let cache_root = test_cache_root();
    fs::create_dir_all(&cache_root).expect("mkdir test fixture cache");
    let blob_path = cache_root.join(format!("{ASSET_SHA256}.tar.gz"));
    if blob_path.exists() && verify_sha256(&blob_path) {
        return blob_path;
    }

    eprintln!("[mailpit_fixture] downloading {ASSET_URL} -> {}", blob_path.display());
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_mins(5))
        .build()
        .expect("reqwest client");
    let resp = client
        .get(ASSET_URL)
        .send()
        .expect("downloading mailpit asset")
        .error_for_status()
        .expect("mailpit asset HTTP non-2xx");
    let bytes = resp.bytes().expect("reading mailpit asset body");

    let actual = sha256_hex(&bytes);
    assert_eq!(
        actual, ASSET_SHA256,
        "mailpit asset sha256 mismatch: got {actual}, expected {ASSET_SHA256}"
    );
    let tmp = blob_path.with_extension("tmp");
    fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(&bytes))
        .expect("writing mailpit asset to cache");
    fs::rename(&tmp, &blob_path).expect("rename mailpit asset into cache");
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
    hex_lower(&hasher.finalize()) == ASSET_SHA256
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

/// Pull the single `mailpit` entry out of the `.tar.gz` and unpack it
/// to `dest`. The archive also carries `LICENSE` + `README.md`, which
/// we skip.
fn extract_mailpit_binary(blob: &Path, dest: &Path) {
    let f = fs::File::open(blob).expect("opening cached mailpit blob");
    let decoder = GzDecoder::new(f);
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);
    for entry in archive.entries().expect("iterating tar entries") {
        let mut entry = entry.expect("reading tar entry");
        let path = entry.path().expect("entry path").into_owned();
        if path.file_name().is_some_and(|n| n == "mailpit")
            && matches!(entry.header().entry_type(), tar::EntryType::Regular)
        {
            entry.unpack(dest).expect("unpacking mailpit binary");
            return;
        }
    }
    panic!("mailpit binary not found inside {}", blob.display());
}
