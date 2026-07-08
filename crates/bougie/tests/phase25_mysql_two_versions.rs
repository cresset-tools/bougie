//! Phase 4 end-to-end: two versions of MySQL running at once against
//! *real* mysqld binaries pulled from the bougie index.
//!
//! MySQL is the first genuinely multi-version service: the index ships
//! 8.4 and 8.0, and a socket-bound DB service coexists purely by
//! version-keyed datadir + socket (no port fallback needed). Two projects
//! pin different majors; both `service up` against one daemon. We assert:
//!
//!   - each instance cold-starts via `mysqld --initialize-insecure` into
//!     its own version-keyed datadir + socket,
//!   - the daemon provisions a database + user + derived password per
//!     project and the per-tenant user authenticates over *its own*
//!     version's socket,
//!   - `service daemon status` reports both `mysql@8.4.10` and
//!     `mysql@8.0.46`,
//!   - the offline env resolver hands each project the socket of the
//!     version it actually runs (8.0 project → 8.0 socket).
//!
//! Skipped under `BOUGIE_SKIP_REAL_MYSQL=1` for CI environments where
//! downloading two ~50 MB tarballs + cold-starting two mysqlds is
//! undesirable.

mod common;

use common::TestEnv;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// Catalog versions the two pins resolve to (must match `daemon::catalog`'s
/// mysql entry). `8.4` → default 8.4.10, `8.0` → 8.0.46.
const MYSQL_8_4: &str = "8.4.10";
const MYSQL_8_0: &str = "8.0.46";

/// Serialise the (heavy) mysql integration test the way phase17 does —
/// two cold-starting mysqlds are enough CPU+IO to matter on a loaded box.
fn mysql_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Generous: each `up` fetches a ~50 MB mysql tarball **and its closure**
/// (zlib/openssl/ncurses/libedit — the client needs libedit at runtime,
/// which is exactly why the real fetch path, not a bare-tarball fixture,
/// is exercised here) from the index, then cold-starts + initialises
/// mysqld.
const STEP_TIMEOUT: Duration = Duration::from_mins(4);

fn should_skip() -> bool {
    std::env::var_os("BOUGIE_SKIP_REAL_MYSQL").is_some()
}

/// A project directory whose basename sanitizes to `tenant`, declaring
/// `mysql` at `version_pin` via `extra.bougie.services`.
fn project_pinning_mysql(parent: &Path, dir: &str, name: &str, version_pin: &str) -> PathBuf {
    let root = parent.join(dir);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("composer.json"),
        format!(
            r#"{{"name":"{name}","extra":{{"bougie":{{"services":{{"mysql":"{version_pin}"}}}}}}}}"#
        ),
    )
    .unwrap();
    root
}

fn up(env: &TestEnv, proj: &Path) {
    env.bougie()
        .args(["service", "up", "--format", "json-v1"])
        .current_dir(proj)
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
}

fn socket_for(env: &TestEnv, version: &str) -> PathBuf {
    env.home_path()
        .join("state/services/mysql")
        .join(version)
        .join("run/mysql.sock")
}

fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// The single tenant row recorded for a version's instance ledger.
fn read_tenant(env: &TestEnv, version: &str) -> Value {
    let ledger = env
        .home_path()
        .join("state/services/mysql")
        .join(version)
        .join("tenants.json");
    let text = fs::read_to_string(&ledger)
        .unwrap_or_else(|e| panic!("reading {}: {e}", ledger.display()));
    let line = text.lines().find(|l| !l.trim().is_empty()).expect("a tenant row");
    serde_json::from_str(line).expect("valid tenant JSON")
}

/// Log in as the tenant user over its own version's socket and run SQL.
fn login_ok(env: &TestEnv, version: &str, user: &str, password: &str) -> String {
    let client = env
        .home_path()
        .join("store")
        .join(format!("mysql-{version}"))
        .join("bin/mysql");
    let out = Command::new(&client)
        .arg("--no-defaults")
        .arg(format!("--socket={}", socket_for(env, version).display()))
        .arg(format!("--user={user}"))
        .arg(format!("--password={password}"))
        .arg(format!("--database={user}"))
        .arg("--batch")
        .arg("--skip-column-names")
        .arg("-e")
        .arg("SELECT DATABASE();")
        .output()
        .expect("running mysql client");
    assert!(
        out.status.success(),
        "mysql {version} login failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn stop_daemon(env: &TestEnv) {
    let _ = env
        .bougie()
        .args(["service", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert();
    std::thread::sleep(Duration::from_millis(500));
}

#[test]
fn two_mysql_versions_run_as_distinct_instances() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_MYSQL set");
        return;
    }
    let _guard = mysql_test_lock();
    let env = TestEnv::new();
    // No pre-staging: `bougie up` fetches each tarball *and its closure*
    // from the index, which is the whole point — the mysql client links
    // libedit, a non-system lib that only lands via `install_closure_peers`.

    let root = env.home_path().join("projects");
    // Project A pins the older major; project B takes the default (8.4).
    let proj_a = project_pinning_mysql(&root, "a", "acme/a", "8.0");
    let proj_b = project_pinning_mysql(&root, "b", "acme/b", "*");

    up(&env, &proj_a);
    up(&env, &proj_b);

    // Each instance cold-started into its own version-keyed socket.
    let sock_a = socket_for(&env, MYSQL_8_0);
    let sock_b = socket_for(&env, MYSQL_8_4);
    assert!(
        wait_for(&sock_a, Duration::from_secs(60)),
        "mysql {MYSQL_8_0} socket never appeared at {}",
        sock_a.display()
    );
    assert!(
        wait_for(&sock_b, Duration::from_secs(60)),
        "mysql {MYSQL_8_4} socket never appeared at {}",
        sock_b.display()
    );

    // Each project's tenant landed in its own version's ledger, and the
    // provisioned user authenticates over that version's socket.
    let ta = read_tenant(&env, MYSQL_8_0);
    let tb = read_tenant(&env, MYSQL_8_4);
    assert_eq!(ta["tenant"], "a");
    assert_eq!(tb["tenant"], "b");
    let pw_a = ta["secrets"]["password"].as_str().expect("password a");
    let pw_b = tb["secrets"]["password"].as_str().expect("password b");
    assert_eq!(login_ok(&env, MYSQL_8_0, "a", pw_a), "a");
    assert_eq!(login_ok(&env, MYSQL_8_4, "b", pw_b), "b");

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
        .filter(|s| s["name"] == "mysql")
        .filter_map(|s| s["version"].as_str())
        .collect();
    assert!(
        versions.contains(&MYSQL_8_0) && versions.contains(&MYSQL_8_4),
        "daemon should report both mysql instances, got {versions:?}"
    );

    // The offline env resolver hands each project *its own* version's
    // socket — proof the ledger-scan (INSTANCES_PLAN §6) works, not just
    // the catalog default.
    let env_a = env
        .bougie()
        .args(["service", "credentials", "mysql", "--env"])
        .current_dir(&proj_a)
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let env_a = String::from_utf8(env_a).unwrap();
    assert!(
        env_a.contains(&format!("/mysql/{MYSQL_8_0}/")),
        "project A's env should point at the 8.0 socket, got:\n{env_a}"
    );

    stop_daemon(&env);
}
