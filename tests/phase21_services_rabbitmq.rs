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
const STEP_TIMEOUT: Duration = Duration::from_secs(3 * 60);

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
    std::thread::sleep(Duration::from_millis(2000));
}

fn services_up_or_dump(env: &TestEnv, proj_path: &Path, extra_args: &[&str]) {
    let mut args = vec!["services", "up"];
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

    if !wait_for_tcp("127.0.0.1:5672", Duration::from_secs(120)) {
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
    if !wait_for_tcp("127.0.0.1:5672", Duration::from_secs(120)) {
        dump_rabbitmq_log(&env, "wait_for_tcp timeout");
        panic!("rabbitmq listener never bound");
    }

    env.bougie()
        .args(["services", "down", "--purge"])
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
    if !wait_for_tcp("127.0.0.1:5672", Duration::from_secs(120)) {
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
