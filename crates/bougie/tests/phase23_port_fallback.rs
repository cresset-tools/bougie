//! Phase 2 end-to-end: the supervisor relocates a service off an
//! occupied catalog port instead of failing to start.
//!
//! Uses a fake Mailpit (`fake-tcp-service`, staged at the catalog store
//! path) so it runs in the fast tier without downloading the real
//! binary. We squat Mailpit's default SMTP port (1025), bring the
//! service up, and assert it landed on a *different* port — recorded in
//! `endpoint.json`, actually bound, and reported by `service
//! credentials`.
//!
//! Gated to run only when the real Mailpit suite (`phase22`) is skipped:
//! both touch 127.0.0.1:1025, and cargo runs test binaries in parallel,
//! so opposite gating keeps them from racing for the port.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::{project_with_composer, TestEnv};
use std::fs;
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// Catalog default version — must match `daemon::catalog`'s mailpit entry.
const MAILPIT_VERSION: &str = "1.30.2";
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

/// Run only in the fast tier (real mailpit skipped), so the two suites
/// don't contend for 127.0.0.1:1025 across test binaries.
fn should_run() -> bool {
    std::env::var_os("BOUGIE_SKIP_REAL_MAILPIT").is_some()
}

/// Stage the fake TCP service as the catalog's `mailpit` binary, the way
/// `phase14` stages fake-redis.
fn install_fake_mailpit(env: &TestEnv) {
    let bin_dir = env
        .home_path()
        .join("store")
        .join(format!("mailpit-{MAILPIT_VERSION}"))
        .join("bin");
    fs::create_dir_all(&bin_dir).expect("mkdir store bin");
    let dst = bin_dir.join("mailpit");
    fs::copy(cargo_bin("fake-tcp-service"), &dst).expect("copy fake-tcp-service");
    fs::set_permissions(&dst, fs::Permissions::from_mode(0o755)).expect("chmod fake mailpit");
}

fn add_and_up(env: &TestEnv, proj: &Path) {
    env.bougie()
        .args(["service", "add", "mailpit"])
        .current_dir(proj)
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["service", "up"])
        .current_dir(proj)
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
}

fn wait_for_tcp(port: u16, timeout: Duration) -> bool {
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

#[test]
fn service_relocates_off_a_squatted_catalog_port() {
    if !should_run() {
        eprintln!("skipping: real mailpit suite active (BOUGIE_SKIP_REAL_MAILPIT unset)");
        return;
    }

    let env = TestEnv::new();
    install_fake_mailpit(&env);
    let proj = project_with_composer("acme/mail");

    // Hold Mailpit's default SMTP port for the whole test. If we can't
    // bind it, something else already owns 1025 — skip rather than flake.
    let Ok(squat) = std::net::TcpListener::bind("127.0.0.1:1025") else {
        eprintln!("skipping: 127.0.0.1:1025 already in use by another process");
        return;
    };

    add_and_up(&env, proj.path());

    // endpoint.json records the *relocated* primary (SMTP) port.
    let ep_path = env
        .home_path()
        .join("state/services/mailpit")
        .join(MAILPIT_VERSION)
        .join("endpoint.json");
    let ep: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&ep_path).expect("endpoint.json exists"))
            .expect("endpoint.json is valid json");
    let primary = u16::try_from(ep["primary"].as_u64().expect("primary port")).unwrap();
    assert_ne!(primary, 1025, "must relocate off the squatted SMTP port");
    assert!(primary > 1025, "allocator scans upward from the default: {primary}");

    // The service genuinely bound the relocated port.
    assert!(
        wait_for_tcp(primary, Duration::from_secs(10)),
        "nothing listening on the relocated port {primary}"
    );

    // `service credentials` hands out the relocated port, not the default.
    let out = env
        .bougie()
        .args(["service", "credentials", "--env"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let env_text = String::from_utf8_lossy(&out);
    assert!(
        env_text.contains(&format!("BOUGIE_SERVICE_MAILPIT_PORT='{primary}'")),
        "credentials should report the relocated port {primary}; got:\n{env_text}"
    );
    assert!(
        !env_text.contains("BOUGIE_SERVICE_MAILPIT_PORT='1025'"),
        "credentials still shows the squatted default port:\n{env_text}"
    );

    // Tear down: stop the daemon, then release the squat.
    let _ = env
        .bougie()
        .args(["service", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert();
    common::wait_for_port_free(primary, Duration::from_secs(30));
    drop(squat);
}
