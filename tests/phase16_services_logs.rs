//! Phase 16: `bougie services logs [-f] [-n N]` end-to-end against the
//! fake-redis fixture. Covers the tail path; follow is covered by a
//! short timed read.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::TestEnv;
use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const STEP_TIMEOUT: Duration = Duration::from_secs(15);

fn install_fake_redis(env: &TestEnv) {
    let store = env.home_path().join("store").join("redis-8.6.3").join("bin");
    fs::create_dir_all(&store).unwrap();
    let dst = store.join("redis-server");
    fs::copy(cargo_bin("fake-redis"), &dst).unwrap();
    use std::os::unix::fs::PermissionsExt;
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
fn logs_tail_shows_lines_the_service_wrote() {
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

    // fake-redis prints "fake-redis: listening on …" once at startup.
    // Give the forwarder a moment to flush the chunk.
    let log_path = env
        .home_path()
        .join("state/services/redis/log/redis.log");
    let deadline = Instant::now() + STEP_TIMEOUT;
    while !log_path.exists() || fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0) == 0 {
        if Instant::now() >= deadline {
            panic!("log file at {} never received bytes", log_path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let out = env
        .bougie()
        .args(["services", "logs", "redis"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("fake-redis"), "expected fake-redis output in tail: {s}");
    stop_daemon(&env);
}

#[test]
fn logs_n_truncates_to_requested_lines() {
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

    // Cheat: append synthetic lines to the log so we have something
    // to truncate. The daemon's forwarder also writes, but we'll just
    // tail enough to see our markers.
    let log_path = env
        .home_path()
        .join("state/services/redis/log/redis.log");
    std::thread::sleep(Duration::from_millis(100)); // let forwarder settle
    let mut text = String::new();
    for i in 0..10 {
        text.push_str(&format!("synthetic-line-{i}\n"));
    }
    use std::io::Write;
    let mut f = fs::OpenOptions::new().append(true).open(&log_path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
    f.sync_all().unwrap();

    let out = env
        .bougie()
        .args(["services", "logs", "-n", "3", "redis"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    // Exactly the last 3 synthetic markers should appear.
    assert!(s.contains("synthetic-line-9"), "{s}");
    assert!(s.contains("synthetic-line-8"), "{s}");
    assert!(s.contains("synthetic-line-7"), "{s}");
    assert!(!s.contains("synthetic-line-6"), "tail spilled past N=3: {s}");
    stop_daemon(&env);
}

#[test]
fn logs_unknown_service_errors_cleanly() {
    let env = TestEnv::new();
    let out = env
        .bougie()
        .args(["services", "logs", "redis"])
        .timeout(STEP_TIMEOUT)
        .assert();
    // The daemon returns ok with an empty tail when the log file
    // doesn't exist (service never started). What we DO want to verify
    // is that an unknown catalog name surfaces as an error.
    drop(out);
    let unknown = env
        .bougie()
        .args(["services", "logs", "postgres"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let s = String::from_utf8(unknown).unwrap();
    assert!(s.contains("not in catalog"), "{s}");
    stop_daemon(&env);
}

#[test]
fn logs_follow_streams_new_bytes_then_ends_on_disconnect() {
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

    // Spawn `bougie services logs -f redis` as a real child so we can
    // SIGTERM it. assert_cmd's `.assert()` waits for completion,
    // which doesn't model follow-mode well.
    let bin = cargo_bin("bougie");
    let child = Command::new(&bin)
        .args(["services", "logs", "-f", "redis"])
        .env("BOUGIE_HOME", env.home_path())
        .env("BOUGIE_CACHE", env.cache_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Give the follow loop time to attach to the file.
    std::thread::sleep(Duration::from_millis(500));

    // Inject a marker through the log file directly.
    let log_path = env
        .home_path()
        .join("state/services/redis/log/redis.log");
    use std::io::Write;
    let mut f = fs::OpenOptions::new().append(true).open(&log_path).unwrap();
    f.write_all(b"FOLLOW-MARKER\n").unwrap();
    f.sync_all().unwrap();

    // Give the daemon's poll loop (250 ms) one cycle to pick it up.
    std::thread::sleep(Duration::from_millis(600));

    // Stop the follow via SIGTERM through rustix (no extra unsafe).
    if let Some(rpid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process(rpid, rustix::process::Signal::TERM);
    }
    let out = child.wait_with_output().expect("waiting for follow child");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("FOLLOW-MARKER"),
        "expected FOLLOW-MARKER in follow output: {s}"
    );
    stop_daemon(&env);
}
