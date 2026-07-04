//! Windows smoke test — phase 6 of `WINDOWS_PLAN.md`.
//!
//! Hits the real `windows.php.net` to verify the end-to-end flow:
//! `bougie init` → `bougie sync` → `bougie ext add xdebug` →
//! `bougie run -- php -m` and confirm xdebug shows up in the module
//! list.
//!
//! Gated `cfg(windows)` because (a) the only path it exercises is the
//! Windows backend's binary-download pipeline and (b) it needs a
//! `php.exe` to actually run. Unix CI never builds it.
//!
//! Runs against the network deliberately — its job is to catch
//! regressions in the real fetch/verify/install path that wiremock
//! tests can't cover (URL shape changes upstream, archive layout
//! drift, hash table going stale).
//!
//! Doesn't share `tests/common/mod.rs` — that module pulls in
//! mariadb/opensearch/rabbitmq fixtures whose hardcoded tarball
//! tables `compile_error!` on Windows. The harness this test needs is
//! tiny enough to inline.

#![cfg(windows)]

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;

struct TestEnv {
    home: TempDir,
    cache: TempDir,
}

impl TestEnv {
    fn new() -> Self {
        Self {
            home: TempDir::new().expect("tempdir for BOUGIE_HOME"),
            cache: TempDir::new().expect("tempdir for BOUGIE_CACHE"),
        }
    }

    fn bougie(&self) -> Command {
        let mut cmd = Command::cargo_bin("bougie").expect("bougie binary");
        cmd.env("BOUGIE_HOME", self.home.path())
            .env("BOUGIE_CACHE", self.cache.path())
            .env_remove("RUST_LOG");
        cmd
    }
}

#[test]
fn install_ext_add_run_shows_extension() {
    let env = TestEnv::new();
    let project = TempDir::new().expect("project tempdir");
    let project_root = project.path();

    env.bougie()
        .arg("init")
        .current_dir(project_root)
        .assert()
        .success();

    // sync downloads PHP from windows.php.net and lays out
    // vendor/bougie/{bin,conf.d,php}/. We deliberately don't pin a version
    // here — the project's freshly-written `"php": "^8.4"` constraint
    // is what we want to exercise, and the backend's resolve step
    // picks whichever 8.4.x windows.php.net currently publishes.
    env.bougie()
        .arg("sync")
        .current_dir(project_root)
        .assert()
        .success();

    // xdebug is a PECL extension on Windows — exercises the
    // single-DLL PECL path (fetch from windows.php.net/downloads/pecl,
    // verify against the baked-in sha256, write 20-xdebug.ini).
    env.bougie()
        .args(["ext", "add", "xdebug"])
        .current_dir(project_root)
        .assert()
        .success();

    // `php -m` loads conf.d via PHP_INI_SCAN_DIR (set by `bougie run`
    // with the Windows `;` separator from the phase-6 fix) and prints
    // every registered module. xdebug must be present — its module
    // header is "Xdebug" (capitalised); accept either casing in case
    // PHP changes its mind.
    env.bougie()
        .args(["run", "--", "php", "-m"])
        .current_dir(project_root)
        .assert()
        .success()
        .stdout(contains("xdebug").or(contains("Xdebug")));
}

/// The telemetry flush lane on Windows: the hidden subcommand is a
/// clean, fast no-op below mode `on` (also exercises the Windows
/// branch of `deprioritize()`, which is a no-op — priority is set by
/// the spawner's creation_flags instead).
#[test]
fn telemetry_flush_noop_below_on() {
    let env = TestEnv::new();
    env.bougie()
        .arg("__telemetry-flush")
        .env("BOUGIE_TELEMETRY", "local")
        .assert()
        .success();
}

/// The detached flush spawn (first `creation_flags` use in the repo:
/// CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP |
/// BELOW_NORMAL_PRIORITY_CLASS) must never delay the parent — even
/// with a heavy spool and a dead collector, the triggering command
/// returns immediately because the parent never waits on the child.
#[test]
fn telemetry_flush_spawn_does_not_delay_the_command() {
    let env = TestEnv::new();
    // Seed a spool heavy enough to trip the size trigger (>64 KiB),
    // dated yesterday-agnostic (size alone is sufficient).
    let spool = env.cache.path().join("telemetry").join("spool");
    std::fs::create_dir_all(&spool).expect("spool dir");
    let line = format!("{{\"schema\":1,\"pad\":\"{}\"}}\n", "x".repeat(1024));
    std::fs::write(spool.join("2026-01-01.ndjson"), line.repeat(80)).expect("seed spool");

    let started = std::time::Instant::now();
    env.bougie()
        .args(["cache", "dir"])
        // Explicit env opt-in (wins over CI detection by design); the
        // endpoint is a dead local port so the child fails silently.
        .env("BOUGIE_TELEMETRY", "on")
        .env("BOUGIE_TELEMETRY_URL", "http://127.0.0.1:9/v1/batch")
        .assert()
        .success();
    // The flush child has a 5s network timeout; the parent must not
    // inherit any of it. A generous bound keeps slow CI runners from
    // flaking while still proving "prompt returns immediately".
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "spawn must be fire-and-forget, took {:?}",
        started.elapsed()
    );
}
