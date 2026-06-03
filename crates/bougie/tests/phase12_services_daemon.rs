//! Phase 12: `bougied` daemon skeleton — auto-spawn, status, version,
//! stop, singleton enforcement. See SERVICES.md §6 and §7.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::TestEnv;
use std::os::unix::process::CommandExt;
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

/// Hard upper bound for any sub-step. Auto-spawn polls at 50ms; 10s is
/// overkill but keeps the suite resilient on cold caches.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

fn wait_for_no_socket(env: &TestEnv) {
    let sock = env.home_path().join("state").join("bougied.sock");
    let deadline = Instant::now() + STEP_TIMEOUT;
    while sock.exists() {
        assert!(
            Instant::now() < deadline,
            "bougied.sock at {} did not disappear within timeout",
            sock.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn status_autospawns_daemon_and_reports_every_catalog_entry_as_stopped() {
    let env = TestEnv::new();

    // First call: daemon isn't running yet. The client must spawn it.
    let out = env
        .bougie()
        .args(["services", "daemon", "status", "--format", "json-v1"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(
        v["socket"],
        env.home_path()
            .join("state")
            .join("bougied.sock")
            .to_str()
            .unwrap()
    );
    assert!(v["pid"].is_number(), "expected pid, got {v}");
    let services = v["services"].as_array().expect("services array");
    assert!(!services.is_empty(), "daemon reports the full catalog");
    let names: Vec<&str> = services
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(names.contains(&"redis"), "{names:?}");
    // Nothing should be running on a fresh daemon.
    for svc in services {
        assert_eq!(svc["state"], "stopped", "{svc}");
    }

    // Clean up so we don't leak a daemon for the next test.
    env.bougie()
        .args(["services", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    wait_for_no_socket(&env);
}

#[test]
fn version_returns_cargo_pkg_version() {
    let env = TestEnv::new();
    let out = env
        .bougie()
        .args(["services", "daemon", "version", "--format", "json-v1"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["daemon"]["version"], env!("CARGO_PKG_VERSION"));

    env.bougie()
        .args(["services", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    wait_for_no_socket(&env);
}

#[test]
fn stop_removes_socket_and_next_status_respawns() {
    let env = TestEnv::new();

    // First status: spawns daemon, socket exists after the call.
    env.bougie()
        .args(["services", "daemon", "status"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let sock = env.home_path().join("state").join("bougied.sock");
    assert!(sock.exists(), "socket should exist after first status");

    // Stop: daemon drains, socket goes away.
    env.bougie()
        .args(["services", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    wait_for_no_socket(&env);

    // Second status: re-spawns a new daemon.
    env.bougie()
        .args(["services", "daemon", "status"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    assert!(sock.exists(), "socket should exist after auto-respawn");

    // Final cleanup.
    env.bougie()
        .args(["services", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    wait_for_no_socket(&env);
}

#[test]
fn stop_when_daemon_not_running_is_idempotent() {
    let env = TestEnv::new();
    // No daemon was ever started; stop should still succeed and say so.
    let out = env
        .bougie()
        .args(["services", "daemon", "stop", "--format", "json-v1"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(
        v["already_stopped"], true,
        "stop with no daemon must report already_stopped; got {v}"
    );
}

#[test]
fn stop_blocks_until_daemon_is_fully_gone() {
    // `daemon stop` must not return until the daemon has drained and
    // released its socket — the whole point of the synchronous stop.
    // We assert the socket is *already* gone the instant the command
    // returns, with no polling grace period.
    let env = TestEnv::new();
    env.bougie()
        .args(["services", "daemon", "status"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let sock = env.home_path().join("state").join("bougied.sock");
    assert!(sock.exists(), "socket should exist after status autospawn");

    let out = env
        .bougie()
        .args(["services", "daemon", "stop", "--format", "json-v1"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    // No `wait_for_no_socket` here on purpose: the contract is that the
    // command already waited.
    assert!(
        !sock.exists(),
        "socket must be gone the moment `stop` returns, not eventually"
    );
    let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(v["already_stopped"], false, "{v}");
}

#[test]
fn second_bougied_fails_to_acquire_singleton_lock() {
    let env = TestEnv::new();

    // First, bring up a daemon via the auto-spawn path.
    env.bougie()
        .args(["services", "daemon", "status"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Now manually try to start a second bougied against the same
    // BOUGIE_HOME — should error out fast because the pid file is
    // flock'd by the first daemon.
    let bin = cargo_bin("bougie");
    let child = StdCommand::new(&bin)
        .arg0("bougied")
        .env("BOUGIE_HOME", env.home_path())
        .env("BOUGIE_CACHE", env.cache_path())
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning second bougied");
    let status = child.wait_with_output().expect("waiting for second bougied");
    assert!(
        !status.status.success(),
        "second bougied must exit non-zero; got {status:?}"
    );
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(
        stderr.contains("already running") || stderr.contains("flock"),
        "expected lock-contention message; got: {stderr}"
    );

    // Clean up.
    env.bougie()
        .args(["services", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    wait_for_no_socket(&env);
}
