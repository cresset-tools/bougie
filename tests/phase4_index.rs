//! Phase 4: index root fetch + signature verification.

use base64::Engine;
use bougie::index::{DetachedEcdsa, FetchOutcome, Verifier, fetch_root};
use eyre::Result;
use sigstore::crypto::signing_key::ecdsa::{ECDSAKeys, EllipticCurve};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Fixture {
    _key_pem: String,
    pubkey_pem: String,
    sig_b64: String,
    root_bytes: Vec<u8>,
    server: MockServer,
}

async fn build_fixture() -> Fixture {
    let keys = ECDSAKeys::new(EllipticCurve::P256).expect("generate P-256");
    let pubkey_pem = keys.as_inner().public_key_to_pem().expect("export pub");
    let signer = keys.to_sigstore_signer().expect("create signer");

    let root_json = serde_json::json!({
        "schema": 1,
        "version": "20260509T000000Z",
        "generated": "2026-05-09T00:00:00Z",
        "targets": {
            "x86_64-unknown-linux-gnu": {
                "sections": {
                    "interpreter/php": { "sha256": "11aa", "size": 100 }
                }
            }
        }
    });
    let root_bytes = serde_json::to_vec(&root_json).unwrap();
    let sig = signer.sign(&root_bytes).expect("sign");
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&sig);

    let server = MockServer::start().await;
    Fixture {
        _key_pem: String::new(),
        pubkey_pem,
        sig_b64,
        root_bytes,
        server,
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// `fetch_root` takes a `FnOnce()` factory now (the verifier is built
/// lazily, only when there's a fresh body to verify). This wraps the
/// detached-ECDSA constructor so each call site stays one line.
fn make_verifier(pem: &[u8]) -> impl FnOnce() -> Result<Box<dyn Verifier>> + use<'_> {
    move || Ok(Box::new(DetachedEcdsa::from_pem(pem)?) as Box<dyn Verifier>)
}

#[test]
fn fetch_root_refreshes_on_first_call() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());

    let etag = "\"v1\"";
    runtime.block_on(async {
        Mock::given(method("GET"))
            .and(path("/index.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(fx.root_bytes.clone())
                    .insert_header("etag", etag),
            )
            .mount(&fx.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/index.json.sig"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(fx.sig_b64.clone()))
            .mount(&fx.server)
            .await;
    });

    let cache = TempDir::new().unwrap();
    let client = reqwest::blocking::Client::new();

    let out = fetch_root(
        &client,
        &fx.server.uri(),
        cache.path(),
        make_verifier(fx.pubkey_pem.as_bytes()),
    )
    .unwrap();
    assert_eq!(out.outcome, FetchOutcome::Refreshed);
    assert_eq!(out.root.schema, 1);

    assert!(cache.path().join("index.json").is_file());
    assert!(cache.path().join("index.json.etag").is_file());
    assert!(cache.path().join("index.json.sig").is_file());
}

#[test]
fn fetch_root_uses_304_on_revalidation() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let etag = "\"abc123\"";

    runtime.block_on(async {
        // Specific first: GET /index.json + If-None-Match=etag → 304.
        // wiremock evaluates mounted mocks in registration order.
        Mock::given(method("GET"))
            .and(path("/index.json"))
            .and(header("if-none-match", etag))
            .respond_with(ResponseTemplate::new(304))
            .mount(&fx.server)
            .await;
        // Catch-all: any other GET /index.json → 200 with body and ETag.
        Mock::given(method("GET"))
            .and(path("/index.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(fx.root_bytes.clone())
                    .insert_header("etag", etag),
            )
            .mount(&fx.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/index.json.sig"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(fx.sig_b64.clone()))
            .mount(&fx.server)
            .await;
    });

    let cache = TempDir::new().unwrap();
    let client = reqwest::blocking::Client::new();

    let first = fetch_root(
        &client,
        &fx.server.uri(),
        cache.path(),
        make_verifier(fx.pubkey_pem.as_bytes()),
    )
    .unwrap();
    assert_eq!(first.outcome, FetchOutcome::Refreshed);

    let second = fetch_root(
        &client,
        &fx.server.uri(),
        cache.path(),
        make_verifier(fx.pubkey_pem.as_bytes()),
    )
    .unwrap();
    assert_eq!(second.outcome, FetchOutcome::Cached);
}

#[test]
fn tampered_body_fails_signature_check() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());

    runtime.block_on(async {
        Mock::given(method("GET"))
            .and(path("/index.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(b"{}".to_vec())
                    .insert_header("etag", "\"x\""),
            )
            .mount(&fx.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/index.json.sig"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(fx.sig_b64.clone()))
            .mount(&fx.server)
            .await;
    });

    let cache = TempDir::new().unwrap();
    let client = reqwest::blocking::Client::new();
    let err = fetch_root(
        &client,
        &fx.server.uri(),
        cache.path(),
        make_verifier(fx.pubkey_pem.as_bytes()),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("could not verify index signature"), "msg: {msg}");
    assert!(msg.contains(&fx.server.uri()), "msg: {msg}");
}

#[test]
fn http_error_maps_to_network_failure() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    runtime.block_on(async {
        Mock::given(method("GET"))
            .and(path("/index.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&fx.server)
            .await;
    });

    let cache = TempDir::new().unwrap();
    let client = reqwest::blocking::Client::new();
    let err = fetch_root(
        &client,
        &fx.server.uri(),
        cache.path(),
        make_verifier(fx.pubkey_pem.as_bytes()),
    )
    .unwrap_err();
    assert!(err.to_string().contains("500"));
}

