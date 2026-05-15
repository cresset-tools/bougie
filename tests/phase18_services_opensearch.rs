//! Phase 18: end-to-end `bougie services up opensearch` against a real
//! opensearch 2.19.5 binary from the bougie index.
//!
//! Coverage:
//!   - the daemon spawns opensearch, the HTTP root binds 127.0.0.1:9200,
//!   - per-tenant index template is created via the daemon's
//!     `opensearch::provision` HTTP path,
//!   - tenants.json records the tenant + reserved index prefix,
//!   - tenant-prefix isolation: project A's indices don't appear when
//!     project B searches across its prefix,
//!   - `services down --purge` drops both the index template and any
//!     indices the tenant created,
//!   - `bougie run` env injection exports BOUGIE_SERVICE_OPENSEARCH_*.
//!
//! Skipped under `BOUGIE_SKIP_REAL_OPENSEARCH=1` for CI environments
//! where downloading the 274 MB tarball is undesirable.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::opensearch_fixture;
use common::TestEnv;
use std::fs;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Serialise opensearch tests within this binary. JVM cold-start is
/// ~15s; running multiple in parallel under `cargo test` saturates
/// CPU and trips the daemon's health probe.
fn opensearch_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Opensearch cold-start dominates timing. JVM bootstrap + cluster
/// state initialisation runs ~15s on a warm cache box.
const STEP_TIMEOUT: Duration = Duration::from_secs(3 * 60);

fn should_skip() -> bool {
    std::env::var_os("BOUGIE_SKIP_REAL_OPENSEARCH").is_some()
}

fn project_with_composer(name: &str) -> TempDir {
    let dir = TempDir::new().expect("project tempdir");
    fs::write(
        dir.path().join("composer.json"),
        format!(r#"{{"name":"{name}"}}"#),
    )
    .unwrap();
    dir
}

fn stop_daemon(env: &TestEnv) {
    let _ = env
        .bougie()
        .args(["services", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert();
    // Give opensearch a chance to wind down before the next test's
    // BOUGIE_HOME (and its 9200 port) tries to come up.
    std::thread::sleep(Duration::from_millis(1500));
}

fn http_get(url: &str) -> (u16, String) {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let resp = client.get(url).send().expect("HTTP GET");
    let status = resp.status().as_u16();
    let body = resp.text().unwrap_or_default();
    (status, body)
}

fn http_put_json(url: &str, body: &str) -> (u16, String) {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let resp = client
        .put(url)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .expect("HTTP PUT");
    let status = resp.status().as_u16();
    let body = resp.text().unwrap_or_default();
    (status, body)
}

fn wait_for_http_root(deadline: Duration) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        if let Ok(r) = client.get("http://127.0.0.1:9200/").send() {
            if r.status().is_success() {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

#[test]
fn up_starts_opensearch_and_provisions_index_template() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_OPENSEARCH set");
        return;
    }
    let _guard = opensearch_test_lock();
    let env = TestEnv::new();
    opensearch_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");

    env.bougie()
        .args(["services", "add", "opensearch"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["services", "up", "--format", "json-v1"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    assert!(
        wait_for_http_root(Duration::from_secs(60)),
        "opensearch HTTP root never responded on 127.0.0.1:9200"
    );

    // Cluster identity sanity check.
    let (status, body) = http_get("http://127.0.0.1:9200/");
    assert_eq!(status, 200, "GET / non-200: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["version"]["number"], "2.19.5");

    // The provisioner persisted a tenant + index_prefix.
    let tenants = env.home_path().join("state/services/opensearch/tenants.json");
    let ledger = fs::read_to_string(&tenants).expect("tenants.json");
    let line = ledger.lines().next().expect("at least one tenant line");
    let t: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(t["tenant"], "acme_blog");
    assert_eq!(t["alloc"]["index_prefix"], "acme_blog-");

    // The index template should be queryable.
    let (status, body) = http_get("http://127.0.0.1:9200/_index_template/acme_blog");
    assert_eq!(status, 200, "GET template non-200: {body}");
    let tmpl: serde_json::Value = serde_json::from_str(&body).unwrap();
    let patterns = &tmpl["index_templates"][0]["index_template"]["index_patterns"];
    assert_eq!(patterns[0], "acme_blog-*", "template body: {body}");

    stop_daemon(&env);
}

#[test]
fn second_up_is_idempotent() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_OPENSEARCH set");
        return;
    }
    let _guard = opensearch_test_lock();
    let env = TestEnv::new();
    opensearch_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["services", "add", "opensearch"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["services", "up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["services", "up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    let ledger = fs::read_to_string(
        env.home_path().join("state/services/opensearch/tenants.json"),
    )
    .unwrap();
    let n = ledger.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(n, 1, "expected one tenant line, got\n{ledger}");

    stop_daemon(&env);
}

#[test]
fn two_projects_have_separate_index_prefixes() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_OPENSEARCH set");
        return;
    }
    let _guard = opensearch_test_lock();
    let env = TestEnv::new();
    opensearch_fixture::install_into(env.home_path());
    let pa = project_with_composer("acme/blog");
    let pb = project_with_composer("acme/store");

    for p in [pa.path(), pb.path()] {
        env.bougie()
            .args(["services", "add", "opensearch"])
            .current_dir(p)
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
        env.bougie()
            .args(["services", "up"])
            .current_dir(p)
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
    }
    assert!(wait_for_http_root(Duration::from_secs(60)));

    // Two templates exist.
    for tenant in ["acme_blog", "acme_store"] {
        let (status, _body) = http_get(&format!(
            "http://127.0.0.1:9200/_index_template/{tenant}"
        ));
        assert_eq!(status, 200, "tenant {tenant} template missing");
    }

    // Project A writes a doc; project B's prefix-scoped search sees zero hits.
    let (s, b) = http_put_json(
        "http://127.0.0.1:9200/acme_blog-posts/_doc/1?refresh=true",
        r#"{"title":"hello"}"#,
    );
    assert!(s == 200 || s == 201, "indexing failed ({s}): {b}");
    let (_, body) = http_get(
        "http://127.0.0.1:9200/acme_store-*/_search?q=*",
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["hits"]["total"]["value"], 0,
        "tenant acme_store should NOT see acme_blog's docs; body: {body}"
    );

    stop_daemon(&env);
}

#[test]
fn down_purge_drops_template_and_indices() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_OPENSEARCH set");
        return;
    }
    let _guard = opensearch_test_lock();
    let env = TestEnv::new();
    opensearch_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["services", "add", "opensearch"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["services", "up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    assert!(wait_for_http_root(Duration::from_secs(60)));

    // Seed an index so we can confirm purge actually deletes it.
    let (s, b) = http_put_json(
        "http://127.0.0.1:9200/acme_blog-posts/_doc/1?refresh=true",
        r#"{"title":"sentinel"}"#,
    );
    assert!(s == 200 || s == 201, "indexing failed ({s}): {b}");
    let (s, _) = http_get("http://127.0.0.1:9200/acme_blog-posts");
    assert_eq!(s, 200, "sentinel index should exist before purge");

    env.bougie()
        .args(["services", "down", "--purge"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Tenant ledger empty.
    let p = env.home_path().join("state/services/opensearch/tenants.json");
    let ledger = fs::read_to_string(&p).unwrap_or_default();
    assert!(
        ledger.lines().all(|l| l.trim().is_empty()),
        "tenants ledger should be empty after --purge; was\n{ledger}"
    );

    // Bring opensearch back up via a different project and verify the
    // dropped index + template are really gone.
    let proj2 = project_with_composer("acme/other");
    env.bougie()
        .args(["services", "add", "opensearch"])
        .current_dir(proj2.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["services", "up"])
        .current_dir(proj2.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    assert!(wait_for_http_root(Duration::from_secs(60)));

    let (s, _) = http_get("http://127.0.0.1:9200/acme_blog-posts");
    assert_eq!(s, 404, "index should be gone after --purge");
    let (s, _) = http_get("http://127.0.0.1:9200/_index_template/acme_blog");
    assert_eq!(s, 404, "template should be gone after --purge");

    stop_daemon(&env);
}

#[test]
fn bougie_run_exports_opensearch_env_vars() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_OPENSEARCH set");
        return;
    }
    let _guard = opensearch_test_lock();
    let env = TestEnv::new();
    opensearch_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["services", "add", "opensearch"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["services", "up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    let bougie_bin = cargo_bin("bougie");
    let out = Command::new(&bougie_bin)
        .args(["run", "--no-sync", "--", "/usr/bin/env"])
        .current_dir(proj.path())
        .env("BOUGIE_HOME", env.home_path())
        .env("BOUGIE_CACHE", env.cache_path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("BOUGIE_SERVICE_OPENSEARCH_URL=http://127.0.0.1:9200"),
        "missing URL var; env was:\n{stdout}"
    );
    assert!(
        stdout.contains("BOUGIE_SERVICE_OPENSEARCH_INDEX_PREFIX=acme_blog-"),
        "missing INDEX_PREFIX var; env was:\n{stdout}"
    );

    stop_daemon(&env);
}
