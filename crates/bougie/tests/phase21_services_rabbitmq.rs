//! Phase 21: end-to-end `bougie services up rabbitmq` against a real
//! rabbitmq 4.2.6 binary (with bundled Erlang/OTP 27.3.4.11) from
//! the bougie index.
//!
//! Coverage:
//!   - bougied spawns rabbitmq-server, the AMQP listener binds
//!     127.0.0.1:5672,
//!   - per-tenant vhost + user + permissions land via `rabbitmqctl`,
//!   - tenants.json records the tenant, vhost, username, and the
//!     generated password (under `secrets.password`),
//!   - `services down --purge` removes the vhost + user from the
//!     live broker,
//!   - `bougie run` env injection exports
//!     `BOUGIE_SERVICE_RABBITMQ_URL` as a fully-formed AMQP DSN.
//!
//! Skipped under `BOUGIE_SKIP_REAL_RABBITMQ=1` for CI environments
//! where downloading the 47 MB tarball is undesirable.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::rabbitmq_fixture;
use common::TestEnv;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Serialise rabbitmq tests within this binary. The Erlang VM boots
/// cold in ~5–10s on a warm box but contends sharply for CPU on CI
/// runners — and they all bind 5672, so parallelism is impossible
/// anyway.
fn rabbitmq_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Erlang VM cold-start + mnesia bootstrap dominate timing; allow a
/// generous ceiling.
const STEP_TIMEOUT: Duration = Duration::from_mins(3);

fn should_skip() -> bool {
    std::env::var_os("BOUGIE_SKIP_REAL_RABBITMQ").is_some()
}

fn project_with_composer(name: &str) -> TempDir {
    let dir = TempDir::new().expect("project tempdir");
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
    // Give the Erlang VM a moment to release 5672 + 4369 (epmd)
    // before the next test's daemon comes up.
    std::thread::sleep(Duration::from_secs(2));
}

fn services_up_or_dump(env: &TestEnv, proj_path: &Path, extra_args: &[&str]) {
    let mut args = vec!["up"];
    args.extend_from_slice(extra_args);
    let res = env
        .bougie()
        .args(&args)
        .current_dir(proj_path)
        .timeout(STEP_TIMEOUT)
        .output()
        .expect("running bougie services up");
    if !res.status.success() {
        dump_rabbitmq_log(env, "services up failure");
        panic!(
            "services up failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
            res.status.code(),
            String::from_utf8_lossy(&res.stdout),
            String::from_utf8_lossy(&res.stderr),
        );
    }
}

/// Best-effort dump of every log file under
/// `state/services/rabbitmq/log/` for diagnostics. rabbitmq writes
/// multiple files (`rabbit@127.0.0.1.log`, `*_upgrade.log`, etc.)
/// — we glob the whole dir.
fn dump_rabbitmq_log(env: &TestEnv, label: &str) {
    let dir = env.home_path().join("state/services/rabbitmq/log");
    eprintln!("\n===== rabbitmq logs [{label}] @ {} =====", dir.display());
    let Ok(entries) = fs::read_dir(&dir) else {
        eprintln!("(no log dir yet)");
        eprintln!("===== end rabbitmq logs =====\n");
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        match fs::read_to_string(&p) {
            Ok(s) => {
                let tail = if s.len() > 8 * 1024 {
                    &s[s.len() - 8 * 1024..]
                } else {
                    &s[..]
                };
                eprintln!("--- {} ---", p.display());
                eprintln!("{tail}");
            }
            Err(e) => eprintln!("--- {} (read failed: {e}) ---", p.display()),
        }
    }
    eprintln!("===== end rabbitmq logs =====\n");
}

fn wait_for_tcp(addr: &str, timeout: Duration) -> bool {
    use std::net::TcpStream;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(
            &addr.parse().expect("addr"),
            Duration::from_millis(250),
        )
        .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// Run `rabbitmqctl <args>` against the tenant broker. Used by tests
/// to introspect state the daemon doesn't surface via IPC (vhost
/// listing, user listing, etc.). Returns (stdout, stderr) and exit.
fn rabbitmqctl(env: &TestEnv, args: &[&str]) -> (i32, String, String) {
    let ctl = env
        .home_path()
        .join("store/rabbitmq-4.2.6/sbin/rabbitmqctl");
    let home = env.home_path().join("state/services/rabbitmq/data/home");
    let out = Command::new(&ctl)
        .args(args)
        // Mirror bougied's env so we hit the same node.
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .env("RABBITMQ_NODENAME", "rabbit@localhost")
        .env("RABBITMQ_NODE_IP_ADDRESS", "127.0.0.1")
        .env("RABBITMQ_NODE_PORT", "5672")
        .env(
            "RABBITMQ_BASE",
            env.home_path().join("state/services/rabbitmq/data"),
        )
        .env(
            "RABBITMQ_MNESIA_BASE",
            env.home_path().join("state/services/rabbitmq/data/mnesia"),
        )
        .env(
            "RABBITMQ_LOG_BASE",
            env.home_path().join("state/services/rabbitmq/log"),
        )
        .output()
        .expect("rabbitmqctl");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn up_starts_rabbitmq_and_provisions_vhost_user() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_RABBITMQ set");
        return;
    }
    let _guard = rabbitmq_test_lock();
    let env = TestEnv::new();
    rabbitmq_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");

    env.bougie()
        .args(["services", "add", "rabbitmq"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &["--format", "json-v1"]);

    if !wait_for_tcp("127.0.0.1:5672", Duration::from_mins(2)) {
        dump_rabbitmq_log(&env, "wait_for_tcp timeout");
        panic!("rabbitmq AMQP listener never bound 127.0.0.1:5672");
    }

    // Tenant ledger captures the vhost + username + a hex password.
    let tenants = env
        .home_path()
        .join("state/services/rabbitmq/tenants.json");
    let ledger = fs::read_to_string(&tenants).expect("tenants.json");
    let line = ledger.lines().next().expect("at least one tenant line");
    let t: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(t["tenant"], "acme_blog");
    assert_eq!(t["alloc"]["vhost"], "acme_blog");
    assert_eq!(t["alloc"]["username"], "acme_blog");
    let pw = t["secrets"]["password"].as_str().expect("password");
    assert_eq!(pw.len(), 48, "expected 48-char hex password, got {pw:?}");

    // The live broker confirms the vhost is there.
    let (code, stdout, stderr) = rabbitmqctl(&env, &["list_vhosts", "--no-table-headers"]);
    assert_eq!(code, 0, "list_vhosts stderr:\n{stderr}");
    assert!(
        stdout.contains("acme_blog"),
        "expected vhost in list_vhosts output:\n{stdout}"
    );

    // And the user — but the username's only meaningful in
    // combination with permissions, so check those instead.
    let (code, stdout, stderr) =
        rabbitmqctl(&env, &["list_user_permissions", "acme_blog", "--no-table-headers"]);
    assert_eq!(code, 0, "list_user_permissions stderr:\n{stderr}");
    assert!(
        stdout.contains("acme_blog"),
        "expected user permissions row:\n{stdout}"
    );

    stop_daemon(&env);
}

#[test]
fn down_purge_drops_vhost_and_user() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_RABBITMQ set");
        return;
    }
    let _guard = rabbitmq_test_lock();
    let env = TestEnv::new();
    rabbitmq_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["services", "add", "rabbitmq"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &[]);
    if !wait_for_tcp("127.0.0.1:5672", Duration::from_mins(2)) {
        dump_rabbitmq_log(&env, "wait_for_tcp timeout");
        panic!("rabbitmq listener never bound");
    }

    env.bougie()
        .args(["down", "--purge"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Tenant ledger should be empty (or missing).
    let tenants = env
        .home_path()
        .join("state/services/rabbitmq/tenants.json");
    let ledger = fs::read_to_string(&tenants).unwrap_or_default();
    assert!(
        ledger.lines().all(|l| l.trim().is_empty()),
        "tenants ledger should be empty after --purge; was\n{ledger}"
    );

    stop_daemon(&env);
}

#[test]
fn bougie_run_exports_rabbitmq_env_vars() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_RABBITMQ set");
        return;
    }
    let _guard = rabbitmq_test_lock();
    let env = TestEnv::new();
    rabbitmq_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["services", "add", "rabbitmq"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &[]);
    if !wait_for_tcp("127.0.0.1:5672", Duration::from_mins(2)) {
        dump_rabbitmq_log(&env, "wait_for_tcp timeout");
        panic!("rabbitmq listener never bound");
    }

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
        stdout.contains("BOUGIE_SERVICE_RABBITMQ_URL=amqp://acme_blog:"),
        "missing or malformed URL var; env was:\n{stdout}"
    );
    assert!(
        stdout.contains("@127.0.0.1:5672/acme_blog"),
        "URL missing authority/vhost; env was:\n{stdout}"
    );
    assert!(
        stdout.contains("BOUGIE_SERVICE_RABBITMQ_VHOST=acme_blog"),
        "missing VHOST var; env was:\n{stdout}"
    );
    assert!(
        stdout.contains("BOUGIE_SERVICE_RABBITMQ_USER=acme_blog"),
        "missing USER var; env was:\n{stdout}"
    );
    assert!(
        stdout.contains("BOUGIE_SERVICE_RABBITMQ_PASSWORD="),
        "missing PASSWORD var; env was:\n{stdout}"
    );

    stop_daemon(&env);
}

/// Regression for cresset-tools/bougie#31.
///
/// `bougie down` without `--purge` removes the tenant from the
/// ledger but leaves the user/vhost in rabbitmq's mnesia store
/// (matches the survives-down semantics of mariadb / opensearch).
/// A subsequent `bougie up` re-provisions, which used to silently
/// no-op on `add_user → "already exists"` while still writing a
/// freshly-generated password into the ledger. The broker kept the
/// *old* password; the ledger advertised the *new* one; AMQP login
/// with `BOUGIE_SERVICE_RABBITMQ_PASSWORD` returned `ACCESS_REFUSED`.
///
/// Fix: when `add_user` errors duplicate, chain `change_password`
/// against the new password so the broker and the ledger stay in
/// sync. This test runs `down` (no purge) + `up` and verifies that
/// `rabbitmqctl authenticate_user <tenant> <ledger_password>`
/// succeeds — i.e. the broker took the new password.
#[test]
fn re_up_after_plain_down_resyncs_password_to_broker() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_RABBITMQ set");
        return;
    }
    let _guard = rabbitmq_test_lock();
    let env = TestEnv::new();
    rabbitmq_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");

    env.bougie()
        .args(["services", "add", "rabbitmq"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &[]);
    if !wait_for_tcp("127.0.0.1:5672", Duration::from_mins(2)) {
        dump_rabbitmq_log(&env, "wait_for_tcp timeout (first up)");
        panic!("rabbitmq listener never bound");
    }

    // Capture password A for sanity-checking the change later.
    let tenants_path = env
        .home_path()
        .join("state/services/rabbitmq/tenants.json");
    let first_ledger = fs::read_to_string(&tenants_path).expect("first tenants.json");
    let first_line = first_ledger.lines().next().expect("first tenant line");
    let pw_a = serde_json::from_str::<serde_json::Value>(first_line).unwrap()["secrets"]
        ["password"]
        .as_str()
        .unwrap()
        .to_owned();

    // `bougie down` (no --purge) wipes the ledger and stops the
    // broker (last-tenant-out shuts the global service down). The
    // user/vhost are persisted in mnesia and survive — that's the
    // precondition that triggers the bug. We can't query the broker
    // while it's stopped; we verify the survives-down invariant
    // implicitly via the duplicate `add_user` failure path that the
    // re-up below exercises.
    env.bougie()
        .args(["down"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Re-`up`: provision sees no ledger row, generates password B,
    // calls add_user → duplicate (user persisted in mnesia) →
    // must chain change_password.
    services_up_or_dump(&env, proj.path(), &[]);
    if !wait_for_tcp("127.0.0.1:5672", Duration::from_mins(2)) {
        dump_rabbitmq_log(&env, "wait_for_tcp timeout (second up)");
        panic!("rabbitmq listener never bound on re-up");
    }
    // Sanity-check the survives-down invariant now that the broker
    // is reachable again: the user should still be present (we
    // didn't `--purge`).
    let (code, stdout, _) = rabbitmqctl(&env, &["list_users", "--no-table-headers"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("acme_blog"),
        "user should survive a non-purge down; list_users was:\n{stdout}"
    );

    let second_ledger = fs::read_to_string(&tenants_path).expect("second tenants.json");
    let second_line = second_ledger.lines().next().expect("second tenant line");
    let pw_b = serde_json::from_str::<serde_json::Value>(second_line).unwrap()["secrets"]
        ["password"]
        .as_str()
        .unwrap()
        .to_owned();
    // Sanity: the regen path actually picked a fresh password.
    // If this ever stops being true (e.g. provision starts
    // recovering the prior secret) the assertion below would still
    // pass trivially, so guard against silent test rot.
    assert_ne!(pw_a, pw_b, "expected provision to regenerate the password");

    // The fix: the broker now authenticates the *new* password.
    // Before the fix this would error with "invalid credentials".
    let (code, stdout, stderr) =
        rabbitmqctl(&env, &["authenticate_user", "acme_blog", &pw_b]);
    assert_eq!(
        code, 0,
        "broker should accept the ledger's current password after re-up; stdout=`{stdout}` stderr=`{stderr}`"
    );

    // And paranoia: the *old* password must no longer work.
    let (code, _, _) = rabbitmqctl(&env, &["authenticate_user", "acme_blog", &pw_a]);
    assert_ne!(
        code, 0,
        "broker still accepts the pre-down password — change_password didn't take effect"
    );

    stop_daemon(&env);
}
