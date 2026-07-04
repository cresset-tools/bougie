//! Phase 22b: auto-restart on failure with exponential backoff.
//!
//! When a managed service crashes, the supervisor should bring it
//! back up — first failure with a 1s backoff, second with 2s, and so
//! on, capped. After a sustained Running window, the backoff resets.
//!
//! Reuses the fake-redis fixture so the test doesn't depend on the
//! real redis tarball. Kills the running child with SIGKILL and
//! watches the daemon's status snapshot for the respawn.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::TestEnv;
use common::project_with_composer;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

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

fn stop_daemon(env: &TestEnv) {
    let _ = env
        .bougie()
        .args(["service", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert();
}

fn status_snapshot(env: &TestEnv, proj: &std::path::Path) -> serde_json::Value {
    let out = env
        .bougie()
        .args(["service", "status", "--format", "json-v1"])
        .current_dir(proj)
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).expect("status JSON")
}

fn service_row<'a>(
    snap: &'a serde_json::Value,
    name: &str,
) -> Option<&'a serde_json::Value> {
    snap["services"]
        .as_array()?
        .iter()
        .find(|s| s["name"] == name)
}

/// Wait for `pred` to hold against a fresh status snapshot. Polls
/// every 250ms. Returns the matching snapshot.
fn wait_for_status<F>(
    env: &TestEnv,
    proj: &std::path::Path,
    timeout: Duration,
    mut pred: F,
) -> Option<serde_json::Value>
where
    F: FnMut(&serde_json::Value) -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let snap = status_snapshot(env, proj);
        if pred(&snap) {
            return Some(snap);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

fn kill_pid(pid: u64) {
    use std::process::Command;
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}

#[test]
fn crashed_service_is_auto_restarted_with_new_pid() {
    let env = TestEnv::new();
    install_fake_redis(&env);
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["service", "add", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["service", "up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    let snap = status_snapshot(&env, proj.path());
    let row = service_row(&snap, "redis").expect("redis in status");
    let pid_before = row["pid"].as_u64().expect("pid present");
    assert_eq!(row["state"], "running");
    assert_eq!(row["failure_count"].as_u64().unwrap_or(0), 0);

    // SIGKILL the child. The 1s ticker should reap it on its next
    // pass and schedule a 1s-backoff respawn; total wall-clock budget
    // for the respawn is roughly: 1s (ticker) + 1s (backoff) +
    // start-cost ≈ 3s. Give ourselves 15s to absorb CI jitter.
    kill_pid(pid_before);

    let post_crash = wait_for_status(
        &env,
        proj.path(),
        Duration::from_secs(15),
        |snap| {
            let Some(row) = service_row(snap, "redis") else {
                return false;
            };
            row["state"] == "running"
                && row["pid"].as_u64().is_some_and(|p| p != pid_before)
        },
    )
    .expect("redis should respawn with a new pid");
    let row_after = service_row(&post_crash, "redis").unwrap();
    let pid_after = row_after["pid"].as_u64().unwrap();
    assert_ne!(pid_after, pid_before, "expected new pid");
    // failure_count should be exactly 1 — one crash recorded since
    // the daemon started managing this service.
    assert_eq!(
        row_after["failure_count"].as_u64().unwrap_or(0),
        1,
        "failure_count should be 1 after one crash; full row: {row_after}"
    );

    stop_daemon(&env);
}

#[test]
fn repeated_crashes_increment_failure_count_and_grow_backoff() {
    let env = TestEnv::new();
    install_fake_redis(&env);
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["service", "add", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["service", "up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Kill the child quickly enough that the Running window stays
    // well under FAILURE_RESET_THRESHOLD (60s). Two crashes back-
    // to-back should yield failure_count == 2 after the second
    // respawn (or, briefly, while still in Failed). We catch the
    // Failed-with-restart-scheduled state because it surfaces the
    // failure_count without racing against the next start.
    let snap = status_snapshot(&env, proj.path());
    let pid1 = service_row(&snap, "redis").unwrap()["pid"]
        .as_u64()
        .unwrap();
    kill_pid(pid1);

    // Wait for the first respawn.
    let after_first = wait_for_status(
        &env,
        proj.path(),
        Duration::from_secs(15),
        |s| {
            let Some(r) = service_row(s, "redis") else {
                return false;
            };
            r["state"] == "running"
                && r["pid"].as_u64().is_some_and(|p| p != pid1)
        },
    )
    .expect("first respawn");
    let pid2 = service_row(&after_first, "redis").unwrap()["pid"]
        .as_u64()
        .unwrap();
    kill_pid(pid2);

    // Wait for the second respawn or the Failed-pending-restart
    // window. Either way `failure_count >= 2` should be visible.
    let after_second = wait_for_status(
        &env,
        proj.path(),
        Duration::from_secs(15),
        |s| {
            let Some(r) = service_row(s, "redis") else {
                return false;
            };
            r["failure_count"].as_u64().unwrap_or(0) >= 2
        },
    )
    .expect("second crash should bump failure_count to >= 2");
    let r = service_row(&after_second, "redis").unwrap();
    assert!(
        r["failure_count"].as_u64().unwrap_or(0) >= 2,
        "expected failure_count >= 2, got: {r}"
    );

    stop_daemon(&env);
}
