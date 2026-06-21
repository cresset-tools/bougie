//! Phase 22: end-to-end `bougie services up mailpit` against a real
//! Mailpit 1.30.2 binary from the upstream GitHub release.
//!
//! Coverage:
//!   - bougied spawns mailpit, the SMTP listener binds 127.0.0.1:1025
//!     and the web UI / REST API binds 127.0.0.1:8025,
//!   - a bare tenant row lands in tenants.json (shared sink — no alloc,
//!     no secrets),
//!   - mail delivered over SMTP is caught and visible via the REST API,
//!   - `bougie run` env injection exports BOUGIE_SERVICE_MAILPIT_HOST /
//!     _PORT / _DSN / _DASHBOARD_URL,
//!   - `services down` removes the tenant and stops the shared instance
//!     once the last tenant goes away.
//!
//! Skipped under `BOUGIE_SKIP_REAL_MAILPIT=1` for CI environments where
//! reaching GitHub for the ~9 MB tarball (or binding 1025/8025) is
//! undesirable.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::mailpit_fixture;
use common::project_with_composer;
use common::TestEnv;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const SMTP_ADDR: &str = "127.0.0.1:1025";
const UI_BASE: &str = "http://127.0.0.1:8025";
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

/// Serialise mailpit tests within this binary: they all bind 1025/8025,
/// so parallel runs would collide on the ports.
fn mailpit_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn should_skip() -> bool {
    std::env::var_os("BOUGIE_SKIP_REAL_MAILPIT").is_some()
}

fn stop_daemon(env: &TestEnv) {
    let _ = env
        .bougie()
        .args(["services", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert();
    // Give mailpit a moment to release 1025/8025 before the next test.
    std::thread::sleep(Duration::from_millis(500));
}

fn add_and_up(env: &TestEnv, proj_path: &Path) {
    env.bougie()
        .args(["services", "add", "mailpit"])
        .current_dir(proj_path)
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["services", "up"])
        .current_dir(proj_path)
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
}

fn wait_for_tcp(addr: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let parsed = addr.parse().expect("addr");
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&parsed, Duration::from_millis(250)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// `GET <UI_BASE><path>` with a short timeout. Returns `(status, body)`.
fn http_get(path: &str) -> Option<(u16, String)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let resp = client.get(format!("{UI_BASE}{path}")).send().ok()?;
    let status = resp.status().as_u16();
    let body = resp.text().ok()?;
    Some((status, body))
}

fn wait_for_http_ok(path: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some((200, _)) = http_get(path) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

// -------------------- minimal SMTP client --------------------

/// Read one SMTP response (handling multi-line `NNN-...` continuations)
/// and return the final reply code.
fn read_reply(reader: &mut impl BufRead) -> Result<u16, String> {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("server closed connection".into());
        }
        let line = line.trim_end();
        if line.len() < 4 {
            return Err(format!("short SMTP reply: {line:?}"));
        }
        let code: u16 = line[..3].parse().map_err(|_| format!("bad code: {line:?}"))?;
        // A '-' in the 4th column marks a continuation line; ' ' is the
        // final line of the reply.
        if line.as_bytes()[3] == b' ' {
            return Ok(code);
        }
    }
}

fn cmd(w: &mut TcpStream, reader: &mut impl BufRead, line: &str, want: u16) -> Result<(), String> {
    write!(w, "{line}\r\n").map_err(|e| format!("write {line:?}: {e}"))?;
    w.flush().map_err(|e| format!("flush: {e}"))?;
    let got = read_reply(reader)?;
    if got != want {
        return Err(format!("after {line:?}: wanted {want}, got {got}"));
    }
    Ok(())
}

/// Speak just enough SMTP to deliver one message. No auth — Mailpit's
/// dev sink accepts any/none.
fn smtp_send(from: &str, to: &str, subject: &str, body: &str) -> Result<(), String> {
    let stream = TcpStream::connect(SMTP_ADDR).map_err(|e| format!("connect: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let mut w = stream.try_clone().map_err(|e| format!("clone: {e}"))?;
    let mut reader = BufReader::new(stream);

    if read_reply(&mut reader)? != 220 {
        return Err("missing 220 greeting".into());
    }
    cmd(&mut w, &mut reader, "EHLO bougie.test", 250)?;
    cmd(&mut w, &mut reader, &format!("MAIL FROM:<{from}>"), 250)?;
    cmd(&mut w, &mut reader, &format!("RCPT TO:<{to}>"), 250)?;
    cmd(&mut w, &mut reader, "DATA", 354)?;
    let message = format!(
        "From: <{from}>\r\nTo: <{to}>\r\nSubject: {subject}\r\n\r\n{body}\r\n.",
    );
    cmd(&mut w, &mut reader, &message, 250)?;
    let _ = cmd(&mut w, &mut reader, "QUIT", 221);
    Ok(())
}

// -------------------- tests --------------------

#[test]
fn up_binds_smtp_and_ui_and_provisions_a_bare_tenant() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_MAILPIT set");
        return;
    }
    let _guard = mailpit_test_lock();
    let env = TestEnv::new();
    mailpit_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");

    add_and_up(&env, proj.path());

    assert!(
        wait_for_tcp(SMTP_ADDR, Duration::from_secs(20)),
        "mailpit SMTP listener never bound {SMTP_ADDR}"
    );
    assert!(
        wait_for_http_ok("/api/v1/info", Duration::from_secs(20)),
        "mailpit web UI / API never answered 200 on {UI_BASE}/api/v1/info"
    );

    // A bare ledger row: the project is present, with no alloc/secrets
    // (shared sink — every project shares one instance).
    let tenants = env.home_path().join("state/services/mailpit/tenants.json");
    let ledger = fs::read_to_string(&tenants).expect("tenants.json should exist");
    let line = ledger.lines().next().expect("one tenant line");
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["tenant"], "acme_blog");
    let expected = fs::canonicalize(proj.path()).unwrap();
    assert_eq!(v["project"], expected.to_str().unwrap());
    assert_eq!(v["alloc"], serde_json::json!({}), "shared sink has no alloc");
    assert_eq!(v["secrets"], serde_json::json!({}), "shared sink has no secrets");

    stop_daemon(&env);
}

#[test]
fn mail_sent_over_smtp_is_caught_and_visible_via_api() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_MAILPIT set");
        return;
    }
    let _guard = mailpit_test_lock();
    let env = TestEnv::new();
    mailpit_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");

    add_and_up(&env, proj.path());
    assert!(
        wait_for_tcp(SMTP_ADDR, Duration::from_secs(20)),
        "mailpit SMTP listener never bound"
    );
    assert!(
        wait_for_http_ok("/api/v1/info", Duration::from_secs(20)),
        "mailpit API never came up"
    );

    let subject = "Bougie Mailpit Smoke Test";
    smtp_send(
        "sender@example.test",
        "rcpt@example.test",
        subject,
        "hello from the bougie test suite",
    )
    .expect("delivering test mail over SMTP");

    // Poll the messages API until our message shows up.
    let deadline = Instant::now() + Duration::from_secs(10);
    let found = loop {
        if let Some((200, body)) = http_get("/api/v1/messages") {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let hit = v["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|m| m["Subject"] == subject);
            if hit {
                break true;
            }
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(found, "test mail never appeared in mailpit's message store");

    stop_daemon(&env);
}

#[test]
fn bougie_run_exports_mailpit_env_vars() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_MAILPIT set");
        return;
    }
    let _guard = mailpit_test_lock();
    let env = TestEnv::new();
    mailpit_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");

    add_and_up(&env, proj.path());
    assert!(
        wait_for_tcp(SMTP_ADDR, Duration::from_secs(20)),
        "mailpit SMTP listener never bound"
    );

    let bougie_bin = cargo_bin("bougie");
    let out = Command::new(&bougie_bin)
        .args(["run", "--no-sync", "--", "/usr/bin/env"])
        .current_dir(proj.path())
        .env("BOUGIE_HOME", env.home_path())
        .env("BOUGIE_CACHE", env.cache_path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    for expected in [
        "BOUGIE_SERVICE_MAILPIT_HOST=127.0.0.1",
        "BOUGIE_SERVICE_MAILPIT_PORT=1025",
        "BOUGIE_SERVICE_MAILPIT_DSN=smtp://127.0.0.1:1025",
        "BOUGIE_SERVICE_MAILPIT_DASHBOARD_URL=http://127.0.0.1:8025",
    ] {
        assert!(stdout.contains(expected), "missing `{expected}`; env was:\n{stdout}");
    }

    stop_daemon(&env);
}

#[test]
fn down_drops_tenant_and_stops_when_last_goes_away() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_MAILPIT set");
        return;
    }
    let _guard = mailpit_test_lock();
    let env = TestEnv::new();
    mailpit_fixture::install_into(env.home_path());
    let a = project_with_composer("acme/blog");
    let b = project_with_composer("acme/store");
    for p in [&a, &b] {
        add_and_up(&env, p.path());
    }
    assert!(wait_for_tcp(SMTP_ADDR, Duration::from_secs(20)), "mailpit never bound");

    // First project goes down — the shared instance must stay up for b.
    env.bougie()
        .args(["services", "down"])
        .current_dir(a.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    assert!(
        wait_for_tcp(SMTP_ADDR, Duration::from_secs(5)),
        "mailpit must keep running while another tenant remains"
    );

    // Last project goes down — now mailpit should stop.
    env.bougie()
        .args(["services", "down"])
        .current_dir(b.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    let out = env
        .bougie()
        .args(["services", "daemon", "status", "--format", "json-v1"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let mailpit = v["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "mailpit")
        .expect("mailpit row");
    assert_eq!(mailpit["state"], "stopped");

    // Ledger emptied.
    let tenants = env.home_path().join("state/services/mailpit/tenants.json");
    let after = fs::read_to_string(&tenants).unwrap_or_default();
    assert!(
        after.lines().all(|l| l.trim().is_empty()),
        "ledger should be empty after both tenants go down: {after:?}"
    );

    stop_daemon(&env);
}
