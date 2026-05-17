//! Real-rabbitmq test fixture.
//!
//! Downloads the rabbitmq-4.2.6 tarball from the bougie index once,
//! caches it under `$HOME/.cache/bougie-test-fixtures/`, and extracts
//! into a per-test `BOUGIE_HOME/store/rabbitmq-4.2.6/`.
//!
//! The tarball ships with its bundled Erlang/OTP at `install/erlang/`,
//! so the test fixture is self-contained — no separate erlang
//! tarball needed at the supervisor layer. (The catalog still lists
//! `erlang` as a `runtime_deps` entry so `bougie services add erlang`
//! works for users who want a standalone Erlang install.)
//!
//! Like every other tool tarball under cresset-tools, contents live
//! under `install/`; we strip the prefix at extraction the same way
//! `src/install.rs` does.

#![allow(dead_code)]

use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Catalog version + tarball name. Must match `daemon::catalog`'s
/// rabbitmq entry exactly so the supervisor's `binary_path` resolver
/// lands on the extracted directory.
pub const RABBITMQ_TARBALL: &str = "rabbitmq-4.2.6";

// Per-target blob coordinates. Pulled from
// `https://index.bougie.tools/versions/r25884606531/targets/<TRIPLE>/manifests/tool/rabbitmq/...`
// — bump both URL and sha together when the index publishes a new tag.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLOB_URL: &str =
    "https://blobs.bougie.tools/blobs/c6/c65bd2548ad642726ec9a57b754e6134a40390407b520ee38d94ffb602a0edfa";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLOB_SHA256: &str = "c65bd2548ad642726ec9a57b754e6134a40390407b520ee38d94ffb602a0edfa";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const BLOB_URL: &str =
    "https://blobs.bougie.tools/blobs/6c/6c0debc455c45d647b9cb7ca5f278d93bc853be6ea290ff54fee732927b5762a";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const BLOB_SHA256: &str = "6c0debc455c45d647b9cb7ca5f278d93bc853be6ea290ff54fee732927b5762a";

#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
)))]
compile_error!(
    "phase21 rabbitmq fixture: no rabbitmq-4.2.6 tarball published for this \
     (os, arch) pair in the bougie index. Add the blob URL + sha256 to \
     tests/common/rabbitmq_fixture.rs."
);

pub fn install_into(bougie_home: &Path) {
    let blob = ensure_blob_cached();
    let dest = bougie_home.join("store").join(RABBITMQ_TARBALL);
    if dest.join("sbin/rabbitmq-server").exists() {
        return;
    }
    fs::create_dir_all(&dest).expect("mkdir rabbitmq store dir");
    extract_zstd_tar_stripping_install_prefix(&blob, &dest);
    assert!(
        dest.join("sbin/rabbitmq-server").exists(),
        "expected sbin/rabbitmq-server at {} after extracting the tarball",
        dest.display()
    );
    assert!(
        dest.join("sbin/rabbitmqctl").exists(),
        "expected sbin/rabbitmqctl at {} after extracting the tarball",
        dest.display()
    );
    assert!(
        dest.join("erlang/bin/erl").exists(),
        "expected bundled erlang at {}/erlang/bin/erl",
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

    eprintln!("[rabbitmq_fixture] downloading {BLOB_URL} -> {}", blob_path.display());
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(5 * 60))
        .build()
        .expect("reqwest client");
    let resp = client
        .get(BLOB_URL)
        .send()
        .expect("downloading rabbitmq blob")
        .error_for_status()
        .expect("rabbitmq blob HTTP non-2xx");
    let bytes = resp.bytes().expect("reading rabbitmq blob body");

    let actual = sha256_hex(&bytes);
    assert_eq!(
        actual, BLOB_SHA256,
        "rabbitmq blob sha256 mismatch: got {actual}, expected {BLOB_SHA256}"
    );
    let tmp = blob_path.with_extension("tmp");
    fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(&bytes))
        .expect("writing rabbitmq blob to cache");
    fs::rename(&tmp, &blob_path).expect("rename rabbitmq blob into cache");
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
    let f = fs::File::open(blob).expect("opening cached rabbitmq blob");
    let decoder = zstd::stream::read::Decoder::new(f).expect("zstd decoder");
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);
    // The rabbitmq tarball contains internal hardlinks under
    // `install/escript/` — `rabbitmq-diagnostics`, `rabbitmq-plugins`,
    // etc. all share inode. `Entry::unpack` reads `link_name` from
    // the tar header verbatim, which still has the `install/` prefix
    // → hard_link errors NotFound. Defer link entries to a second
    // pass after every regular file has landed, rewriting the link
    // target to the post-strip path.
    let mut pending_links: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in archive.entries().expect("iterating tar entries") {
        let mut entry = entry.expect("reading tar entry");
        let path = entry.path().expect("entry path").into_owned();
        let mut comps = path.components();
        let first = comps.next();
        if !first.is_some_and(|c| c.as_os_str() == "install") {
            continue;
        }
        let rest: PathBuf = comps.collect();
        if rest.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(&rest);

        if matches!(entry.header().entry_type(), tar::EntryType::Link) {
            // Hardlink: stash for the second pass.
            let link_in_tar = entry
                .header()
                .link_name()
                .expect("link_name")
                .expect("hardlink without link_name");
            let stripped: PathBuf = link_in_tar
                .strip_prefix("install")
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| link_in_tar.into_owned());
            let link_target = dest.join(&stripped);
            pending_links.push((link_target, target));
        } else {
            entry.unpack(&target).expect("unpacking entry");
        }
    }
    for (target, link_path) in pending_links {
        if let Some(parent) = link_path.parent() {
            fs::create_dir_all(parent).expect("mkdir for hardlink parent");
        }
        // Copy rather than hard_link so subsequent test runs with a
        // different tempdir aren't bound to an existing inode in the
        // cache. The duplication is ~tens of KB across the tree.
        fs::copy(&target, &link_path).unwrap_or_else(|e| {
            panic!(
                "copying hardlink target {} -> {}: {}",
                target.display(),
                link_path.display(),
                e
            )
        });
    }
}
