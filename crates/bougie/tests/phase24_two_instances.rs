//! Phase 1b end-to-end: two versions of one service run at once.
//!
//! The whole point of instance-keying the supervisor: `mysql 8.0` beside
//! `mysql 8.4`, or (here) two Mailpit versions. Two projects each pin a
//! different exact version of `mailpit`; both `service up` against the
//! same daemon. We assert the daemon materialized *two distinct
//! instances* — separate version-keyed state dirs, separate `endpoint.json`
//! primary ports (relocated apart by the allocator), both actually
//! listening — and that `service daemon status` reports both.
//!
//! Uses the fake `fake-tcp-service` (Mailpit-shaped) staged at two store
//! versions, so it runs in the fast tier without downloading anything.
//! Gated to the fast tier (real mailpit skipped) like `phase23`, since it
//! contends for the same low ports. It never asserts *exact* ports (the
//! allocator's landing spot depends on what else holds 1025+), only that
//! the two instances landed on *different* live ports.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::TestEnv;
use serde_json::Value;
use std::fs;
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// Catalog default (project A). Must match `daemon::catalog`'s mailpit entry.
const V_DEFAULT: &str = "1.30.2";
/// A second, non-default exact version (project B) — only has to be a
/// valid `store/mailpit-<v>/` dir name; the fake binary is version-agnostic.
const V_OTHER: &str = "9.9.9";
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

/// Run only in the fast tier (real mailpit skipped), same as `phase23`.
fn should_run() -> bool {
    std::env::var_os("BOUGIE_SKIP_REAL_MAILPIT").is_some()
}

/// Stage the fake TCP service as `store/mailpit-<version>/bin/mailpit`.
fn stage_fake_mailpit(env: &TestEnv, version: &str) {
    let bin_dir = env
        .home_path()
        .join("store")
        .join(format!("mailpit-{version}"))
        .join("bin");
    fs::create_dir_all(&bin_dir).expect("mkdir store bin");
    let dst = bin_dir.join("mailpit");
    fs::copy(cargo_bin("fake-tcp-service"), &dst).expect("copy fake-tcp-service");
    fs::set_permissions(&dst, fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// A project declaring `mailpit` at an exact version pin.
fn project_pinning_mailpit(dir: &Path, name: &str, version_pin: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("composer.json"),
        format!(
            r#"{{"name":"{name}","extra":{{"bougie":{{"services":{{"mailpit":"{version_pin}"}}}}}}}}"#
        ),
    )
    .unwrap();
}

fn up(env: &TestEnv, proj: &Path) {
    env.bougie()
        .args(["service", "up"])
        .current_dir(proj)
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
}

fn endpoint_primary(env: &TestEnv, version: &str) -> u16 {
    let ep = env
        .home_path()
        .join("state/services/mailpit")
        .join(version)
        .join("endpoint.json");
    let v: Value = serde_json::from_str(
        &fs::read_to_string(&ep).unwrap_or_else(|e| panic!("endpoint.json for {version}: {e}")),
    )
    .expect("valid endpoint.json");
    u16::try_from(v["primary"].as_u64().expect("primary")).unwrap()
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
fn two_versions_of_one_service_run_as_distinct_instances() {
    if !should_run() {
        eprintln!("skipping: real mailpit suite active (BOUGIE_SKIP_REAL_MAILPIT unset)");
        return;
    }

    let env = TestEnv::new();
    stage_fake_mailpit(&env, V_DEFAULT);
    stage_fake_mailpit(&env, V_OTHER);

    // Two projects, same shared daemon, each pinning a different version.
    let root = env.home_path().join("projects");
    let proj_a = root.join("a");
    let proj_b = root.join("b");
    project_pinning_mailpit(&proj_a, "acme/a", "*"); // -> catalog default 1.30.2
    project_pinning_mailpit(&proj_b, "acme/b", V_OTHER); // -> exact 9.9.9

    up(&env, &proj_a);
    up(&env, &proj_b);

    // Each instance recorded its own version-keyed endpoint on a distinct
    // primary port (the allocator relocates the second off the first).
    let p_default = endpoint_primary(&env, V_DEFAULT);
    let p_other = endpoint_primary(&env, V_OTHER);
    assert_ne!(
        p_default, p_other,
        "two instances must land on different ports ({p_default} vs {p_other})"
    );

    // Both are genuinely listening — two live processes, not one.
    assert!(
        wait_for_tcp(p_default, Duration::from_secs(10)),
        "mailpit {V_DEFAULT} not listening on {p_default}"
    );
    assert!(
        wait_for_tcp(p_other, Duration::from_secs(10)),
        "mailpit {V_OTHER} not listening on {p_other}"
    );

    // The daemon reports both instances — same name, distinct versions.
    let out = env
        .bougie()
        .args(["service", "daemon", "status", "--format", "json-v1"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).expect("valid JSON");
    let versions: Vec<&str> = v["services"]
        .as_array()
        .expect("services array")
        .iter()
        .filter(|s| s["name"] == "mailpit")
        .filter_map(|s| s["version"].as_str())
        .collect();
    assert!(
        versions.contains(&V_DEFAULT) && versions.contains(&V_OTHER),
        "daemon should report both mailpit instances, got {versions:?}"
    );

    // Tear down the daemon; release the ports for the next test.
    let _ = env
        .bougie()
        .args(["service", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert();
    common::wait_for_port_free(p_default, Duration::from_secs(30));
    common::wait_for_port_free(p_other, Duration::from_secs(30));
}
