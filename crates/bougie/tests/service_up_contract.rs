//! What `bougie service up` promises when it exits 0.
//!
//! Namely: the services it was asked for are up. The interesting case is
//! a *concurrent* start — the reaper's auto-restart of a crash-looping
//! service, or a second `up` — because the supervisor has nothing to
//! spawn there and used to report success on the spot, while the start
//! it saw in flight was seconds from failing.
//!
//! Driven by a stub service binary that sleeps (so the window is real
//! and not a race to lose) and then exits non-zero. Unix-only: the
//! services stack is.
#![cfg(unix)]

mod common;

use common::TestEnv;
use common::project_with_composer;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the stub stays alive before failing. Long enough that a
/// second `up` reliably lands while the first is still health-checking,
/// short enough to keep the test quick.
const STUB_LINGER: Duration = Duration::from_secs(3);

/// Put a service binary at redis's catalog path that never binds its
/// port and exits non-zero — a service that cannot come up, without
/// needing a real one.
fn install_failing_service(env: &TestEnv) {
    use std::os::unix::fs::PermissionsExt;
    let store = env
        .home_path()
        .join("store")
        .join("redis-8.6.3")
        .join("bin");
    fs::create_dir_all(&store).expect("mkdir store");
    let dst = store.join("redis-server");
    fs::write(
        &dst,
        format!(
            "#!/bin/sh\n# stub: linger without binding, then fail\nsleep {}\nexit 1\n",
            STUB_LINGER.as_secs()
        ),
    )
    .expect("write stub");
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

/// `bougie service up`, exit code and merged output.
fn service_up(env: &TestEnv, proj: &Path) -> (i32, String) {
    let out = env
        .bougie()
        .args(["service", "up"])
        .current_dir(proj)
        .timeout(STEP_TIMEOUT)
        .output()
        .expect("running bougie service up");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

#[test]
fn up_fails_when_the_service_cannot_start() {
    let env = TestEnv::new();
    install_failing_service(&env);
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["service", "add", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    let (code, out) = service_up(&env, proj.path());
    assert_ne!(code, 0, "up must not report success; output was:\n{out}");

    stop_daemon(&env);
}

#[test]
fn up_does_not_report_success_for_a_start_already_in_flight() {
    // The regression: with a start in flight the supervisor had nothing
    // to spawn, and returned "already running" without waiting to see
    // whether that start worked. Against a crash-looping service that's
    // the common case, not a rare race — every `up` for the seconds a
    // doomed boot takes exits 0.
    let env = TestEnv::new();
    install_failing_service(&env);
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["service", "add", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // First `up` — takes the stub's linger plus the probe, and fails.
    let first = Command::new(assert_cmd::cargo::cargo_bin("bougie"))
        .args(["service", "up"])
        .current_dir(proj.path())
        .env("BOUGIE_HOME", env.home_path())
        .env("BOUGIE_CACHE", env.cache_path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the first up");

    // Land the second one inside that window, while the first start is
    // still health-checking.
    std::thread::sleep(STUB_LINGER / 2);
    let (code, out) = service_up(&env, proj.path());

    let mut first = first;
    let _ = first.wait();
    assert_ne!(
        code, 0,
        "up landed on a start in flight that then failed, so it must not \
         report success; output was:\n{out}"
    );

    stop_daemon(&env);
}
