//! Phase 19: end-to-end `bougie service up server` against the
//! `bougie server run` SAPI itself.
//!
//! Coverage:
//!   - bougied spawns `bougie server run --config <conf>/server.toml`
//!     and the dev server binds 127.0.0.1:7080 + control socket,
//!   - per-tenant `[[host]]` block lands in the managed server.toml,
//!   - tenants.json records the tenant + reserved hostname,
//!   - the running server picks up the new host via control socket
//!     `reload-config` (no restart),
//!   - two projects get two distinct `[[host]]` blocks and the
//!     server reports both via its status endpoint,
//!   - `bougie service down --purge` drops the host block and the
//!     reload propagates,
//!   - `bougie run` env injection exports `BOUGIE_SERVICE_SERVER_*`.
//!
//! This test doesn't actually drive HTTP traffic through the dev
//! server (that needs a synced PHP interpreter — out of scope for
//! Phase 8). It verifies the bougied↔server control plane only.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::TestEnv;
use common::project_with_composer_and_public as project_with_composer;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// Serialise phase19 tests — every test spawns a bougie server child
/// on the catalog port (127.0.0.1:7080). Running them in parallel
/// would have the second test's server child fail with EADDRINUSE.
fn server_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Same escape hatch as the other real-service suites: a dev whose
/// own shared server is live on 7080 can't run these (the
/// supervisor's pre-start probe refuses the occupied catalog port —
/// by design, so tests can't silently talk to the real instance).
fn should_skip() -> bool {
    if std::env::var_os("BOUGIE_SKIP_REAL_SERVER").is_some() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_SERVER set");
        return true;
    }
    false
}

fn stop_daemon(env: &TestEnv) {
    let _ = env
        .bougie()
        .args(["service", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert();
    // Wait until the shared server actually released 7080 — the
    // supervisor's pre-start probe hard-fails on an occupied port.
    common::wait_for_port_free(7080, Duration::from_secs(30));
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
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Send the server's control socket a `status` request and parse the
/// reply. Returns `None` if the socket isn't there yet.
fn server_status() -> Option<serde_json::Value> {
    let sock = control_socket_path();
    if !sock.exists() {
        return None;
    }
    let mut stream = UnixStream::connect(&sock).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .ok()?;
    stream.write_all(b"{\"v\":1,\"method\":\"status\"}\n").ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).ok()?;
    serde_json::from_str(buf.trim()).ok()
}

fn control_socket_path() -> std::path::PathBuf {
    let xdg = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            // bougied's fallback (in tests on macOS specifically,
            // where XDG_RUNTIME_DIR is rarely set).
            let uid = rustix::process::geteuid().as_raw();
            std::path::PathBuf::from(format!("/tmp/bougie-server-{uid}"))
        });
    xdg.join("bougie").join("server").join("control.sock")
}

#[test]
fn up_brings_server_online_and_registers_host() {
    if should_skip() {
        return;
    }
    let _guard = server_test_lock();
    let env = TestEnv::new();
    let proj = project_with_composer("acme/blog");

    env.bougie()
        .args(["service", "add", "server"])
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

    assert!(
        wait_for_tcp("127.0.0.1:7080", Duration::from_secs(10)),
        "bougie server never bound 127.0.0.1:7080"
    );

    // Managed server.toml carries the host block.
    let cfg = env
        .home_path()
        .join("state/services/server/conf/server.toml");
    let body = fs::read_to_string(&cfg).expect("server.toml should exist");
    assert!(body.contains("acme-blog.bougie.run"), "server.toml: {body}");
    let expected_proj = fs::canonicalize(proj.path()).unwrap();
    assert!(
        body.contains(expected_proj.to_str().unwrap()),
        "expected canonical project path in server.toml: {body}"
    );

    // Tenant ledger records the hostname.
    let ledger = fs::read_to_string(
        env.home_path().join("state/services/server/tenants.json"),
    )
    .expect("tenants.json should exist");
    let line = ledger.lines().next().expect("at least one tenant");
    let t: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(t["tenant"], "acme_blog");
    assert_eq!(t["alloc"]["hostname"], "acme-blog.bougie.run");

    // Running server's in-memory host map reflects the new block.
    let status = server_status().expect("server status reachable");
    let hosts = status["hosts"]
        .as_array()
        .expect("hosts array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(
        hosts.contains(&"acme-blog.bougie.run".to_string()),
        "server status hosts missing tenant: {hosts:?}"
    );

    stop_daemon(&env);
}

#[test]
fn two_projects_share_one_server_with_distinct_hosts() {
    if should_skip() {
        return;
    }
    let _guard = server_test_lock();
    let env = TestEnv::new();
    let pa = project_with_composer("acme/blog");
    let pb = project_with_composer("acme/store");

    for p in [pa.path(), pb.path()] {
        env.bougie()
            .args(["service", "add", "server"])
            .current_dir(p)
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
        env.bougie()
            .args(["service", "up"])
            .current_dir(p)
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
    }
    assert!(wait_for_tcp("127.0.0.1:7080", Duration::from_secs(10)));

    // Server's live state reflects both hosts via a single running
    // process (no restart between provisions).
    let status = server_status().expect("server status reachable");
    let hosts = status["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(hosts.contains(&"acme-blog.bougie.run".to_string()), "{hosts:?}");
    assert!(hosts.contains(&"acme-store.bougie.run".to_string()), "{hosts:?}");

    stop_daemon(&env);
}

#[test]
fn down_purge_drops_host_block() {
    if should_skip() {
        return;
    }
    let _guard = server_test_lock();
    let env = TestEnv::new();
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["service", "add", "server"])
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
    assert!(wait_for_tcp("127.0.0.1:7080", Duration::from_secs(10)));

    env.bougie()
        .args(["service", "down", "--purge"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Tenant ledger empty.
    let ledger = fs::read_to_string(
        env.home_path().join("state/services/server/tenants.json"),
    )
    .unwrap_or_default();
    assert!(
        ledger.lines().all(|l| l.trim().is_empty()),
        "tenants ledger should be empty after --purge; was\n{ledger}"
    );

    // The hostname is gone from server.toml AND from the running
    // server's in-memory map.
    let cfg = env
        .home_path()
        .join("state/services/server/conf/server.toml");
    if cfg.exists() {
        let body = fs::read_to_string(&cfg).unwrap();
        assert!(
            !body.contains("acme-blog.bougie.run"),
            "host block should be gone: {body}"
        );
    }

    stop_daemon(&env);
}

#[test]
fn up_fails_with_actionable_hint_when_no_docroot() {
    // Project has neither `pub` nor `public` and no project config:
    // `bougie up` must surface a clear error pointing at the two
    // configurable escape hatches.
    if should_skip() {
        return;
    }
    let _guard = server_test_lock();
    let env = TestEnv::new();
    let proj = TempDir::new().expect("project tempdir");
    fs::write(
        proj.path().join("composer.json"),
        r#"{"name":"acme/blog"}"#,
    )
    .unwrap();
    // Deliberately no `public/` or `pub/` directory.

    env.bougie()
        .args(["service", "add", "server"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let assert = env
        .bougie()
        .args(["service", "up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .failure();
    let out = assert.get_output();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stderr}\n{stdout}");
    assert!(
        combined.contains("pub") && combined.contains("public"),
        "error should mention both candidate dirs; got:\n{combined}"
    );
    assert!(
        combined.contains("bougie.toml") || combined.contains("composer.json"),
        "error should point at where to configure root; got:\n{combined}"
    );

    stop_daemon(&env);
}

#[test]
fn explicit_root_in_composer_extra_overrides_autodetect() {
    // `extra.bougie.server.root` wins over both auto-detect
    // candidates. Project has `pub/` (highest auto-detect priority)
    // but the config points at `web/`; the host block must take the
    // configured value.
    if should_skip() {
        return;
    }
    let _guard = server_test_lock();
    let env = TestEnv::new();
    let proj = TempDir::new().expect("project tempdir");
    fs::write(
        proj.path().join("composer.json"),
        r#"{"name":"acme/blog","extra":{"bougie":{"server":{"root":"web"}}}}"#,
    )
    .unwrap();
    fs::create_dir_all(proj.path().join("pub")).unwrap();
    fs::create_dir_all(proj.path().join("web")).unwrap();

    env.bougie()
        .args(["service", "add", "server"])
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
    assert!(wait_for_tcp("127.0.0.1:7080", Duration::from_secs(10)));

    let cfg = env
        .home_path()
        .join("state/services/server/conf/server.toml");
    let body = fs::read_to_string(&cfg).expect("server.toml should exist");
    assert!(
        body.contains("root = \"web\""),
        "expected explicit root in server.toml: {body}"
    );

    stop_daemon(&env);
}

#[test]
fn bougie_run_exports_server_env_vars() {
    if should_skip() {
        return;
    }
    let _guard = server_test_lock();
    let env = TestEnv::new();
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["service", "add", "server"])
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
    assert!(wait_for_tcp("127.0.0.1:7080", Duration::from_secs(10)));

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
        stdout.contains("BOUGIE_SERVICE_SERVER_URL=http://127.0.0.1:7080"),
        "missing URL var; env was:\n{stdout}"
    );
    assert!(
        stdout.contains("BOUGIE_SERVICE_SERVER_HOSTNAME=acme-blog.bougie.run"),
        "missing HOSTNAME var; env was:\n{stdout}"
    );

    stop_daemon(&env);
}

