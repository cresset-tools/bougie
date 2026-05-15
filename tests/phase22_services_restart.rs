//! Phase 22: `bougie services restart` end-to-end.
//!
//! Restart should:
//!   - replace the live service process (new PID),
//!   - leave the tenant ledger untouched (no rotated passwords or
//!     DB numbers — apps that have them cached keep working),
//!   - report the restarted service in its result frame.
//!
//! Uses the same fake-redis fixture as phase14 so this test doesn't
//! depend on the real redis tarball being on the CDN.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::TestEnv;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tempfile::TempDir;

const STEP_TIMEOUT: Duration = Duration::from_secs(15);

fn install_fake_redis(env: &TestEnv) {
    let store = env.home_path().join("store/redis-8.6.3/bin");
    fs::create_dir_all(&store).expect("mkdir store");
    let dst = store.join("redis-server");
    fs::copy(cargo_bin("fake-redis"), &dst).expect("copy fake-redis");
    let mut perms = fs::metadata(&dst).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&dst, perms).unwrap();
}

fn project_with_composer(name: &str) -> TempDir {
    let dir = TempDir::new().expect("project tempdir");
    let json = format!(r#"{{"name":"{name}"}}"#);
    fs::write(dir.path().join("composer.json"), json).unwrap();
    dir
}

fn stop_daemon(env: &TestEnv) {
    let _ = env
        .bougie()
        .args(["services", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert();
}

fn read_pid(env: &TestEnv, proj: &std::path::Path, service: &str) -> Option<u64> {
    let out = env
        .bougie()
        .args(["services", "status", "--format", "json-v1"])
        .current_dir(proj)
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).ok()?;
    v["services"]
        .as_array()?
        .iter()
        .find(|s| s["name"] == service)?["pid"]
        .as_u64()
}

#[test]
fn restart_replaces_the_process_and_preserves_the_tenant() {
    let env = TestEnv::new();
    install_fake_redis(&env);
    let proj = project_with_composer("acme/blog");

    env.bougie()
        .args(["services", "add", "redis"])
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

    let pid_before = read_pid(&env, proj.path(), "redis").expect("redis should be running");
    let ledger_before =
        fs::read_to_string(env.home_path().join("state/services/redis/tenants.json")).unwrap();

    let out = env
        .bougie()
        .args(["services", "restart", "--format", "json-v1"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let restarted = v["restarted"].as_array().expect("restarted array");
    assert!(
        restarted.iter().any(|s| s == "redis"),
        "restarted list missing redis: {v}"
    );

    // New PID means the supervisor really cycled the child.
    let pid_after = read_pid(&env, proj.path(), "redis").expect("redis should be running after restart");
    assert_ne!(
        pid_after, pid_before,
        "restart should have produced a new pid (was {pid_before}, still {pid_after})"
    );

    // Tenant ledger byte-identical: no re-provision happened.
    let ledger_after =
        fs::read_to_string(env.home_path().join("state/services/redis/tenants.json")).unwrap();
    assert_eq!(
        ledger_before, ledger_after,
        "tenant ledger must be preserved across restart"
    );

    stop_daemon(&env);
}

#[test]
fn restart_of_stopped_service_is_a_noop() {
    let env = TestEnv::new();
    install_fake_redis(&env);
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["services", "add", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Service was added but never started: restart should report
    // nothing restarted, exit 0.
    let out = env
        .bougie()
        .args(["services", "restart", "--format", "json-v1"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v["restarted"].as_array().unwrap().len(),
        0,
        "restart of stopped service should be a no-op: {v}"
    );

    stop_daemon(&env);
}

#[test]
fn restart_of_undeclared_service_errors() {
    let env = TestEnv::new();
    install_fake_redis(&env);
    let proj = project_with_composer("acme/blog");
    // Don't `services add` redis. Asking to restart it should fail
    // before any IPC fires.
    let assertion = env
        .bougie()
        .args(["services", "restart", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("isn't declared"),
        "expected actionable error, got: {stderr}"
    );

    stop_daemon(&env);
}
