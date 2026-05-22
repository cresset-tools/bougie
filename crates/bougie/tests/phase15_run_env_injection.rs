//! Phase 15: `bougie run` injects per-tenant `BOUGIE_SERVICE_*` env
//! vars into the child process when `bougied` is alive.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::TestEnv;
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

const STEP_TIMEOUT: Duration = Duration::from_secs(15);

fn install_fake_redis(env: &TestEnv) {
    use std::os::unix::fs::PermissionsExt;
    let store = env.home_path().join("store").join("redis-8.6.3").join("bin");
    fs::create_dir_all(&store).unwrap();
    let dst = store.join("redis-server");
    fs::copy(cargo_bin("fake-redis"), &dst).unwrap();
    let mut perms = fs::metadata(&dst).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&dst, perms).unwrap();
}

fn project_with_composer(name: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
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
}

#[test]
fn run_exports_service_env_after_services_up() {
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
        .args(["up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    let out = env
        .bougie()
        .args(["run", "--no-sync", "--", "/usr/bin/env"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    let sock_line = s
        .lines()
        .find(|l| l.starts_with("BOUGIE_SERVICE_REDIS_SOCKET="))
        .unwrap_or_else(|| panic!("expected BOUGIE_SERVICE_REDIS_SOCKET in env: {s}"));
    assert!(sock_line.contains("redis.sock"), "{sock_line}");
    assert!(
        s.lines().any(|l| l == "BOUGIE_SERVICE_REDIS_DB=0"),
        "expected BOUGIE_SERVICE_REDIS_DB=0 in env: {s}"
    );
    stop_daemon(&env);
}

#[test]
fn run_without_daemon_running_skips_service_env_silently() {
    // No `services up` — the daemon was never spawned and the socket
    // doesn't exist. `bougie run` must succeed and emit no
    // `BOUGIE_SERVICE_*` vars.
    let env = TestEnv::new();
    let proj = project_with_composer("acme/blog");
    let out = env
        .bougie()
        .args(["run", "--no-sync", "--", "/usr/bin/env"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(
        !s.lines().any(|l| l.starts_with("BOUGIE_SERVICE_")),
        "expected no BOUGIE_SERVICE_* vars when daemon is absent: {s}"
    );
    // bougied.sock should still not exist (we did not auto-spawn).
    let sock = env.home_path().join("state").join("bougied.sock");
    assert!(!sock.exists(), "`bougie run` must not auto-spawn bougied");
}

#[test]
fn run_after_services_down_no_longer_exports_redis_vars() {
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
        .args(["up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["down"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    let out = env
        .bougie()
        .args(["run", "--no-sync", "--", "/usr/bin/env"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(
        !s.lines().any(|l| l.starts_with("BOUGIE_SERVICE_REDIS")),
        "expected no BOUGIE_SERVICE_REDIS_* after `services down`: {s}"
    );
    stop_daemon(&env);
}

#[test]
fn two_projects_get_distinct_redis_db_numbers_in_env() {
    let env = TestEnv::new();
    install_fake_redis(&env);
    let a = project_with_composer("acme/blog");
    let b = project_with_composer("acme/store");
    for p in [&a, &b] {
        env.bougie()
            .args(["services", "add", "redis"])
            .current_dir(p.path())
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
        env.bougie()
            .args(["up"])
            .current_dir(p.path())
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
    }

    let env_var = |p: &TempDir, var: &str| -> String {
        let out = env
            .bougie()
            .args(["run", "--no-sync", "--", "/usr/bin/env"])
            .current_dir(p.path())
            .timeout(STEP_TIMEOUT)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let s = String::from_utf8(out).unwrap();
        s.lines()
            .find_map(|l| l.strip_prefix(&format!("{var}=")))
            .unwrap_or_else(|| panic!("missing {var}: {s}"))
            .to_string()
    };

    assert_eq!(env_var(&a, "BOUGIE_SERVICE_REDIS_DB"), "0");
    assert_eq!(env_var(&b, "BOUGIE_SERVICE_REDIS_DB"), "1");
    stop_daemon(&env);
}
