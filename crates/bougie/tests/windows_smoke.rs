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
    // .bougie/{bin,conf.d,php}/. We deliberately don't pin a version
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
