//! Phase 6: end-to-end `bougie php install` / `list` / `find` /
//! `uninstall` against a wiremock-served fake index.

use base64::Engine;
use predicates::str::contains;
use sha2::{Digest, Sha256};
use sigstore::crypto::signing_key::ecdsa::{ECDSAKeys, EllipticCurve};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::TestEnv;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Build a tar.zst blob containing `bin/php` (a stub script) inside
/// the install root. Returns (bytes, sha256).
fn build_php_tarball() -> (Vec<u8>, String) {
    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        let script = b"#!/bin/sh\necho fake-php\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("bin/php").unwrap();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, &script[..]).unwrap();
        builder.finish().unwrap();
    }
    let zst = zstd::stream::encode_all(&tar_buf[..], 0).unwrap();
    let h = hex(&zst);
    (zst, h)
}

#[allow(clippy::too_many_lines)]
fn host_target() -> String {
    bougie::Triple::detect().unwrap().to_string()
}

struct Fixture {
    server: MockServer,
    pub_pem: String,
    blob_sha: String,
}

async fn build_fixture() -> Fixture {
    let keys = ECDSAKeys::new(EllipticCurve::P256).unwrap();
    let pub_pem = keys.as_inner().public_key_to_pem().unwrap();
    let signer = keys.to_sigstore_signer().unwrap();

    let server = MockServer::start().await;
    let target = host_target();

    let (blob_bytes, blob_sha) = build_php_tarball();
    let blob_url = format!("{}/blobs/{}", server.uri(), &blob_sha);

    let manifest_path_abs = format!(
        "/targets/{}/manifests/php/8.3/php-8.3.12-{}-nts.json",
        target, target
    );
    let manifest_json = serde_json::json!({
        "schema": 1,
        "kind": "interpreter",
        "name": "php",
        "tag": format!("php-8.3.12-{target}-nts"),
        "version": "8.3.12",
        "target": target,
        "flavor": "nts",
        "abi": {"php":"8.3","zend_module_api_no":"20230831","zend_extension_api_no":"420230831"},
        "libc": {"family":"gnu","min":"2.17"},
        "blob": {"url": blob_url, "sha256": blob_sha},
        "closure": [],
        "sapis": ["cli","fpm"]
    });
    let manifest_bytes = serde_json::to_vec(&manifest_json).unwrap();
    let manifest_sha = hex(&manifest_bytes);

    let section_json = serde_json::json!({
        "schema": 1,
        "name": "php",
        "kind": "interpreter",
        "target": target,
        "artifacts": [{
            "tag": format!("php-8.3.12-{target}-nts"),
            "version": "8.3.12",
            "flavor": "nts",
            "manifest": {"path": manifest_path_abs, "sha256": manifest_sha},
            "yanked": false,
            "frozen": false
        }]
    });
    let section_bytes = serde_json::to_vec(&section_json).unwrap();
    let section_sha = hex(&section_bytes);

    let root_json = serde_json::json!({
        "schema": 1,
        "generated": "2026-05-09T00:00:00Z",
        "targets": {
            target.clone(): {
                "sections": {
                    "interpreter/php": {"sha256": section_sha, "size": section_bytes.len()}
                }
            }
        }
    });
    let root_bytes = serde_json::to_vec(&root_json).unwrap();
    let sig = signer.sign(&root_bytes).unwrap();
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&sig);

    let section_path = format!("/targets/{target}/sections/interpreter/php.json");
    let manifest_path = manifest_path_abs.clone();
    let blob_path = format!("/blobs/{blob_sha}");

    Mock::given(method("GET")).and(path("/index.json"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(root_bytes).insert_header("etag", "\"v1\""))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/index.json.sig"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(sig_b64.into_bytes()))
        .mount(&server).await;
    Mock::given(method("GET")).and(path(section_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(section_bytes))
        .mount(&server).await;
    Mock::given(method("GET")).and(path(manifest_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(manifest_bytes))
        .mount(&server).await;
    Mock::given(method("GET")).and(path(blob_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(blob_bytes))
        .mount(&server).await;

    Fixture { server, pub_pem, blob_sha }
}

fn write_trust_root(env: &TestEnv, pem: &str) -> std::path::PathBuf {
    let p = env.home_path().join("test-trust-root.pub");
    std::fs::write(&p, pem).unwrap();
    p
}

#[test]
fn install_then_list_then_find_then_uninstall() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();
    let trust_path = write_trust_root(&env, &fx.pub_pem);

    // install
    env.bougie()
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust_path)
        .args(["php", "install", "8.3.12"])
        .assert()
        .success()
        .stdout(contains("installed php 8.3.12-nts"));

    // list shows it
    env.bougie()
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust_path)
        .args(["php", "list"])
        .assert()
        .success()
        .stdout(contains("8.3.12"))
        .stdout(contains("nts"));

    // find returns the bin/php path
    let out = env
        .bougie()
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust_path)
        .args(["php", "find"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let line = String::from_utf8(out).unwrap();
    let path_str = line.trim();
    assert!(path_str.ends_with("8.3.12-nts/bin/php"), "got: {path_str}");

    // re-install is idempotent ("already")
    env.bougie()
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust_path)
        .args(["php", "install", "8.3.12"])
        .assert()
        .success()
        .stdout(contains("already php 8.3.12-nts"));

    // uninstall
    env.bougie()
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust_path)
        .args(["php", "uninstall", "8.3.12"])
        .assert()
        .success()
        .stdout(contains("removed"));

    // list now empty
    env.bougie()
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust_path)
        .args(["php", "list"])
        .assert()
        .success()
        .stdout(contains("no PHP interpreters installed"));

    let _ = fx.blob_sha; // hush unused
}

#[test]
fn find_with_no_install_errors() {
    let env = TestEnv::new();
    env.bougie()
        .args(["php", "find"])
        .assert()
        .failure()
        .stderr(contains("no PHP interpreter installed"));
}
