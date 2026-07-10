//! Phase 18: end-to-end `bougie service up opensearch` against a real
//! opensearch 2.19.5 binary from the bougie index.
//!
//! Coverage:
//!   - the daemon spawns opensearch, the HTTP root binds 127.0.0.1:9200,
//!   - per-tenant index template is created via the daemon's
//!     `opensearch::provision` HTTP path,
//!   - tenants.json records the tenant + reserved index prefix,
//!   - tenant-prefix isolation: project A's indices don't appear when
//!     project B searches across its prefix,
//!   - `service down --purge` drops both the index template and any
//!     indices the tenant created,
//!   - `bougie run` env injection exports `BOUGIE_SERVICE_OPENSEARCH_*`.
//!
//! Skipped under `BOUGIE_SKIP_REAL_OPENSEARCH=1` for CI environments
//! where downloading the 274 MB tarball is undesirable.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::opensearch_fixture;
use common::project_with_composer;
use common::TestEnv;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

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
const STEP_TIMEOUT: Duration = Duration::from_mins(3);

fn should_skip() -> bool {
    std::env::var_os("BOUGIE_SKIP_REAL_OPENSEARCH").is_some()
}

fn stop_daemon(env: &TestEnv) {
    let _ = env
        .bougie()
        .args(["service", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert();
    // Wait until opensearch actually released 9200 — the supervisor's
    // pre-start probe hard-fails on an occupied port, so the next
    // test's `up` needs the listener genuinely gone, not "probably
    // gone after a nap".
    common::wait_for_port_free(9200, Duration::from_secs(30));
}

/// Run `bougie service up` for the project and panic on failure
/// AFTER dumping opensearch.log + the bougied call's stderr — so a
/// CI failure surfaces the real JVM/sandbox error instead of just
/// the supervisor's "TCP-connect never won" rollup.
fn services_up_or_dump(env: &TestEnv, proj_path: &Path, extra_args: &[&str]) {
    let mut args = vec!["service", "up"];
    args.extend_from_slice(extra_args);
    let res = env
        .bougie()
        .args(&args)
        .current_dir(proj_path)
        .timeout(STEP_TIMEOUT)
        .output()
        .expect("running bougie service up");
    if !res.status.success() {
        dump_opensearch_log(env, "services up failure");
        panic!(
            "services up failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
            res.status.code(),
            String::from_utf8_lossy(&res.stdout),
            String::from_utf8_lossy(&res.stderr),
        );
    }
}

/// Dump opensearch.log to stderr so a `... did not start accepting
/// connections within ...` failure on CI shows the actual JVM error
/// (sandbox denial, JNA extraction failure, etc.) rather than just
/// the supervisor's "TCP-connect never won" rollup. Best-effort.
fn dump_opensearch_log(env: &TestEnv, label: &str) {
    let p = env.home_path().join("state/services/opensearch/2.19.5/log/opensearch.log");
    eprintln!("\n===== opensearch.log [{label}] @ {} =====", p.display());
    match fs::read_to_string(&p) {
        Ok(s) => {
            // Limit to the last ~8 KB so test output stays readable.
            let tail = if s.len() > 8 * 1024 {
                &s[s.len() - 8 * 1024..]
            } else {
                &s[..]
            };
            eprintln!("{tail}");
        }
        Err(e) => eprintln!("(could not read: {e})"),
    }
    eprintln!("===== end opensearch.log =====\n");
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
        .args(["service", "add", "opensearch"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &["--format", "json-v1"]);

    if !wait_for_http_root(Duration::from_mins(1)) {
        dump_opensearch_log(&env, "wait_for_http_root timeout");
        panic!("opensearch HTTP root never responded on 127.0.0.1:9200");
    }

    // Cluster identity sanity check.
    let (status, body) = http_get("http://127.0.0.1:9200/");
    assert_eq!(status, 200, "GET / non-200: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["version"]["number"], "2.19.5");

    // The provisioner persisted a tenant + index_prefix.
    let tenants = env.home_path().join("state/services/opensearch/2.19.5/tenants.json");
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
        .args(["service", "add", "opensearch"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &[]);
    services_up_or_dump(&env, proj.path(), &[]);

    let ledger = fs::read_to_string(
        env.home_path().join("state/services/opensearch/2.19.5/tenants.json"),
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
            .args(["service", "add", "opensearch"])
            .current_dir(p)
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
        services_up_or_dump(&env, p, &[]);
    }
    if !wait_for_http_root(Duration::from_mins(1)) {
        dump_opensearch_log(&env, "wait_for_http_root timeout (two_projects)");
        panic!("opensearch HTTP root never responded");
    }

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
        .args(["service", "add", "opensearch"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &[]);
    if !wait_for_http_root(Duration::from_mins(1)) {
        dump_opensearch_log(&env, "wait_for_http_root timeout (down_purge)");
        panic!("opensearch HTTP root never responded");
    }

    // Seed an index so we can confirm purge actually deletes it.
    let (s, b) = http_put_json(
        "http://127.0.0.1:9200/acme_blog-posts/_doc/1?refresh=true",
        r#"{"title":"sentinel"}"#,
    );
    assert!(s == 200 || s == 201, "indexing failed ({s}): {b}");
    let (s, _) = http_get("http://127.0.0.1:9200/acme_blog-posts");
    assert_eq!(s, 200, "sentinel index should exist before purge");

    env.bougie()
        .args(["service", "down", "--purge"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Tenant ledger empty.
    let p = env.home_path().join("state/services/opensearch/2.19.5/tenants.json");
    let ledger = fs::read_to_string(&p).unwrap_or_default();
    assert!(
        ledger.lines().all(|l| l.trim().is_empty()),
        "tenants ledger should be empty after --purge; was\n{ledger}"
    );

    // Bring opensearch back up via a different project and verify the
    // dropped index + template are really gone.
    let proj2 = project_with_composer("acme/other");
    env.bougie()
        .args(["service", "add", "opensearch"])
        .current_dir(proj2.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj2.path(), &[]);
    if !wait_for_http_root(Duration::from_mins(1)) {
        dump_opensearch_log(&env, "wait_for_http_root timeout (post-purge re-up)");
        panic!("opensearch HTTP root never responded");
    }

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
        .args(["service", "add", "opensearch"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &[]);

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
