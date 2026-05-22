//! Phase 20: daemon self-restart on version mismatch.
//!
//! `bougie self update` lands a new CLI binary while a daemon from
//! the previous version is still running. The next IPC call detects
//! the mismatch, sends `daemon.shutdown`, waits for the socket to
//! disappear, and lets the regular auto-spawn path bring up a fresh
//! daemon at the new version. End-to-end behavior should be
//! transparent to the user except for a single "(restarting bougied:
//! …)" line on stderr.
//!
//! `BOUGIE_VERSION_OVERRIDE` is the test seam: both the CLI's
//! `cli_version()` and the daemon's `daemon.version` response read it,
//! so a one-shot env override on a single CLI invocation can simulate
//! "older daemon than CLI" without rebuilding the binary.

mod common;

use common::TestEnv;
use std::time::{Duration, Instant};

const STEP_TIMEOUT: Duration = Duration::from_secs(15);

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

/// Read the bougied PID file. Returns the trimmed string; tests
/// compare equality, so the integer value isn't load-bearing.
fn read_pid(env: &TestEnv) -> String {
    std::fs::read_to_string(env.home_path().join("state/bougied.pid"))
        .expect("bougied.pid should exist while daemon is running")
        .trim()
        .to_string()
}

/// Ask the running daemon what version it thinks it is. Uses the
/// daemon-control bypass (`caller_method` == "daemon.version"), so it
/// doesn't trigger a recursive upgrade check.
fn read_daemon_version(env: &TestEnv) -> String {
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
    v["daemon"]["version"]
        .as_str()
        .expect("daemon.version string")
        .to_string()
}

#[test]
fn cli_restarts_daemon_when_version_mismatches() {
    let env = TestEnv::new();

    // 1. Spawn a daemon under a fake older version. The CLI's own
    // override matches (so no immediate restart), and the autospawned
    // daemon inherits the env, so it also reports the fake version.
    env.bougie()
        .args(["services", "daemon", "status"])
        .env("BOUGIE_VERSION_OVERRIDE", "0.0.1-old")
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Confirm the staged daemon reports the fake version. We pass the
    // same override on the read so the CLI's version check sees a
    // match and doesn't tear the daemon down before we read it.
    let staged_pid = read_pid(&env);
    let out = env
        .bougie()
        .args(["services", "daemon", "version", "--format", "json-v1"])
        .env("BOUGIE_VERSION_OVERRIDE", "0.0.1-old")
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["daemon"]["version"], "0.0.1-old");

    // 2. Run a CLI command WITHOUT the override. The CLI reports the
    // real CARGO_PKG_VERSION; the running daemon still reports
    // 0.0.1-old (its env was captured at spawn time). Mismatch →
    // CLI sends daemon.shutdown, waits, autospawns a new daemon with
    // no override, which reports the real version.
    let assertion = env
        .bougie()
        .args(["services", "daemon", "status"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("restarting bougied"),
        "expected upgrade notice on stderr; got: {stderr}"
    );

    // 3. The new daemon reports the binary's real version, and its
    // PID is different from the staged one (proof of actual respawn).
    assert_eq!(read_daemon_version(&env), env!("CARGO_PKG_VERSION"));
    let fresh_pid = read_pid(&env);
    assert_ne!(
        staged_pid, fresh_pid,
        "respawned daemon should have a new PID"
    );

    // Cleanup.
    env.bougie()
        .args(["services", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    wait_for_no_socket(&env);
}

#[test]
fn cli_does_not_restart_when_versions_match() {
    let env = TestEnv::new();

    // First call autospawns the daemon at the real CARGO_PKG_VERSION.
    env.bougie()
        .args(["services", "daemon", "status"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let pid1 = read_pid(&env);

    // Subsequent calls should not respawn; PID stays put.
    for _ in 0..3 {
        env.bougie()
            .args(["services", "daemon", "status"])
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
    }
    let pid2 = read_pid(&env);
    assert_eq!(pid1, pid2, "daemon must not respawn when versions match");

    env.bougie()
        .args(["services", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    wait_for_no_socket(&env);
}

#[test]
fn daemon_version_subcommand_skips_the_upgrade_check() {
    // Running `services daemon version` against a fake-old daemon
    // should report the daemon's reported version verbatim — never
    // restart it. (That subcommand is the operator's tool for seeing
    // what's actually running, so it must bypass the auto-upgrade.)
    let env = TestEnv::new();
    env.bougie()
        .args(["services", "daemon", "status"])
        .env("BOUGIE_VERSION_OVERRIDE", "0.0.1-old")
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let pid_before = read_pid(&env);

    let out = env
        .bougie()
        .args(["services", "daemon", "version", "--format", "json-v1"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v["daemon"]["version"], "0.0.1-old",
        "daemon version subcommand must report the live daemon's version, not restart"
    );
    assert_eq!(
        read_pid(&env),
        pid_before,
        "`daemon version` must not respawn even on version mismatch"
    );

    env.bougie()
        .args(["services", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    wait_for_no_socket(&env);
}
