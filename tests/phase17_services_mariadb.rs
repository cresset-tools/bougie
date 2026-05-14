//! Phase 17: end-to-end `bougie services up mariadb` against a *real*
//! mariadb 11.4.4 binary pulled from the bougie index.
//!
//! Coverage:
//!   - cold-start bootstrap via `mariadb-install-db` lands a usable datadir,
//!   - a unix socket appears at the catalog-specified path,
//!   - the daemon provisions a database + user + password and persists
//!     them in the `tenants.json` ledger,
//!   - the per-tenant user can authenticate over the socket,
//!   - cross-tenant isolation: tenant A cannot reach tenant B's database,
//!   - `services down --purge` drops the DB + user, leaving the data
//!     directory but removing the tenant record,
//!   - `bougie run` env injection surfaces BOUGIE_SERVICE_MARIADB_*.
//!
//! Skipped under `BOUGIE_SKIP_REAL_MARIADB=1` for CI environments where
//! downloading the 25 MB tarball is undesirable.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::mariadb_fixture;
use common::TestEnv;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Serialise mariadb integration tests. Each test cold-starts mariadbd
/// (~3–5s of CPU+IO), and running five of them in parallel under
/// `cargo test` reliably blows past the daemon's 60s health-probe
/// window on a loaded box. The lock has no semantic meaning — it's
/// just throttling.
fn mariadb_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        // A poisoned lock here means a previous test panicked; that
        // panic already failed the test, so just take the guard back.
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Generous because cold-start `mariadb-install-db` can take ~5s and a
/// first mariadbd start takes ~3s on a warm cache box; CI under load
/// can push these higher.
const STEP_TIMEOUT: Duration = Duration::from_secs(2 * 60);

fn should_skip() -> bool {
    std::env::var_os("BOUGIE_SKIP_REAL_MARIADB").is_some()
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
    // Give the daemon a beat to release the mariadbd child it owns —
    // its drain path SIGTERMs running services. Subsequent tests rely
    // on the datadir's mysql lock files being released.
    std::thread::sleep(Duration::from_millis(500));
}

/// Read the mariadb client binary out of the test fixture so we can
/// run SQL against the running service without going through `bougie run`.
fn mariadb_client(env: &TestEnv) -> std::path::PathBuf {
    env.home_path()
        .join("store")
        .join(mariadb_fixture::MARIADB_TARBALL)
        .join("bin/mariadb")
}

fn mariadb_socket(env: &TestEnv) -> std::path::PathBuf {
    env.home_path()
        .join("state/services/mariadb/run/mariadb.sock")
}

fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

fn read_tenant(env: &TestEnv) -> serde_json::Value {
    let p = env.home_path().join("state/services/mariadb/tenants.json");
    let ledger = fs::read_to_string(&p).expect("tenants.json should exist");
    let line = ledger.lines().next().expect("at least one tenant");
    serde_json::from_str(line).expect("tenant record is JSON")
}

#[test]
fn up_bootstraps_mariadb_and_provisions_a_tenant() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_MARIADB set");
        return;
    }
    let _guard = mariadb_test_lock();
    let env = TestEnv::new();
    mariadb_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");

    env.bougie()
        .args(["services", "add", "mariadb"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    env.bougie()
        .args(["services", "up", "--format", "json-v1"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    let sock = mariadb_socket(&env);
    assert!(
        wait_for(&sock, Duration::from_secs(30)),
        "mariadb socket never appeared at {}",
        sock.display()
    );

    let v = read_tenant(&env);
    assert_eq!(v["tenant"], "acme_blog");
    let expected = fs::canonicalize(proj.path()).unwrap();
    assert_eq!(v["project"], expected.to_str().unwrap());
    let pw = v["secrets"]["password"].as_str().expect("password recorded");
    assert_eq!(pw.len(), 32, "password should be 32-char hex");

    // The provisioned user can actually log in and see its database.
    // `--no-defaults` for the same reason the daemon's client uses it:
    // CI runners ship a system /etc/my.cnf full of MySQL-8-only options.
    let out = Command::new(mariadb_client(&env))
        .arg("--no-defaults")
        .arg(format!("--socket={}", sock.display()))
        .arg("--user=acme_blog")
        .arg(format!("--password={pw}"))
        .arg("--database=acme_blog")
        .arg("--batch")
        .arg("--skip-column-names")
        .arg("-e")
        .arg("SELECT DATABASE();")
        .output()
        .expect("running mariadb client");
    assert!(
        out.status.success(),
        "mariadb client failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.trim() == "acme_blog",
        "expected SELECT DATABASE() == acme_blog, got `{stdout}`"
    );

    stop_daemon(&env);
}

#[test]
fn second_up_is_idempotent_no_duplicate_tenant() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_MARIADB set");
        return;
    }
    let _guard = mariadb_test_lock();
    let env = TestEnv::new();
    mariadb_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["services", "add", "mariadb"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["services", "up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["services", "up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    let ledger = fs::read_to_string(
        env.home_path().join("state/services/mariadb/tenants.json"),
    )
    .unwrap();
    let n = ledger.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(n, 1, "expected single tenant line, got\n{ledger}");
    stop_daemon(&env);
}

#[test]
fn two_projects_get_isolated_databases() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_MARIADB set");
        return;
    }
    let _guard = mariadb_test_lock();
    let env = TestEnv::new();
    mariadb_fixture::install_into(env.home_path());
    let pa = project_with_composer("acme/blog");
    let pb = project_with_composer("acme/store");

    for p in [pa.path(), pb.path()] {
        env.bougie()
            .args(["services", "add", "mariadb"])
            .current_dir(p)
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
        env.bougie()
            .args(["services", "up"])
            .current_dir(p)
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
    }

    let ledger = fs::read_to_string(
        env.home_path().join("state/services/mariadb/tenants.json"),
    )
    .unwrap();
    let lines: Vec<_> = ledger.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 2, "expected two tenants: {ledger}");
    let names: Vec<_> = lines
        .iter()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["tenant"].as_str().unwrap().to_string()
        })
        .collect();
    assert!(names.contains(&"acme_blog".to_string()));
    assert!(names.contains(&"acme_store".to_string()));

    // Tenant A cannot use B's database.
    let pw_a = {
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        v["secrets"]["password"].as_str().unwrap().to_string()
    };
    let other_db = if names[0] == "acme_blog" { "acme_store" } else { "acme_blog" };
    let out = Command::new(mariadb_client(&env))
        .arg("--no-defaults")
        .arg(format!("--socket={}", mariadb_socket(&env).display()))
        .arg(format!("--user={}", names[0]))
        .arg(format!("--password={pw_a}"))
        .arg(format!("--database={other_db}"))
        .arg("-e")
        .arg("SELECT 1;")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "tenant `{}` should NOT be able to USE `{other_db}` (it succeeded). stdout={}",
        names[0],
        String::from_utf8_lossy(&out.stdout),
    );

    stop_daemon(&env);
}

#[test]
fn down_purge_drops_database_and_user() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_MARIADB set");
        return;
    }
    let _guard = mariadb_test_lock();
    let env = TestEnv::new();
    mariadb_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["services", "add", "mariadb"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["services", "up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Insert a sentinel row so we can prove the data is really gone
    // (not just the user record).
    let v = read_tenant(&env);
    let pw = v["secrets"]["password"].as_str().unwrap().to_string();
    let mariadb = mariadb_client(&env);
    let sock = mariadb_socket(&env);
    let out = Command::new(&mariadb)
        .arg("--no-defaults")
        .arg(format!("--socket={}", sock.display()))
        .arg("--user=acme_blog")
        .arg(format!("--password={pw}"))
        .arg("--database=acme_blog")
        .arg("-e")
        .arg("CREATE TABLE sentinel (id INT); INSERT INTO sentinel VALUES (42);")
        .output()
        .unwrap();
    assert!(out.status.success(), "create+insert: {}", String::from_utf8_lossy(&out.stderr));

    env.bougie()
        .args(["services", "down", "--purge"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Tenant record is gone.
    let p = env.home_path().join("state/services/mariadb/tenants.json");
    let ledger = fs::read_to_string(&p).unwrap_or_default();
    assert!(
        ledger.lines().all(|l| l.trim().is_empty()),
        "tenants ledger should be empty after --purge; was\n{ledger}"
    );

    // Bring mariadb back up to confirm root can no longer see the DB.
    let proj2 = project_with_composer("acme/other");
    env.bougie()
        .args(["services", "add", "mariadb"])
        .current_dir(proj2.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["services", "up"])
        .current_dir(proj2.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let sock = mariadb_socket(&env);
    assert!(wait_for(&sock, Duration::from_secs(30)));
    // Connect as the OS user — mariadb-install-db's socket-auth
    // mode maps that user onto the unix_socket root account.
    let os_user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "bougie".into());
    let out = Command::new(&mariadb)
        .arg("--no-defaults")
        .arg(format!("--socket={}", sock.display()))
        .arg(format!("--user={os_user}"))
        .arg("--batch")
        .arg("--skip-column-names")
        .arg("-e")
        .arg("SHOW DATABASES LIKE 'acme_blog';")
        .output()
        .unwrap();
    assert!(out.status.success(), "show databases: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.trim().is_empty(),
        "expected acme_blog dropped, but SHOW returned: `{stdout}`"
    );

    stop_daemon(&env);
}

#[test]
fn bougie_run_exports_mariadb_env_vars() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_MARIADB set");
        return;
    }
    let _guard = mariadb_test_lock();
    let env = TestEnv::new();
    mariadb_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["services", "add", "mariadb"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["services", "up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // `bougie run -- env` reads the env injected from the daemon.
    // We don't go through composer-managed php here — the run wrapper
    // can exec arbitrary argv per CLI.md §3.4.
    let bougie_bin = cargo_bin("bougie");
    let out = Command::new(&bougie_bin)
        .args(["run", "--no-sync", "--", "/usr/bin/env"])
        .current_dir(proj.path())
        .env("BOUGIE_HOME", env.home_path())
        .env("BOUGIE_CACHE", env.cache_path())
        .output()
        .unwrap();
    if !out.status.success() {
        eprintln!(
            "bougie run failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let socket_path = mariadb_socket(&env).display().to_string();
    assert!(
        stdout.contains(&format!("BOUGIE_SERVICE_MARIADB_SOCKET={socket_path}")),
        "missing socket var; env was:\n{stdout}"
    );
    assert!(
        stdout.contains("BOUGIE_SERVICE_MARIADB_DATABASE=acme_blog"),
        "missing database var; env was:\n{stdout}"
    );
    assert!(
        stdout.contains("BOUGIE_SERVICE_MARIADB_USER=acme_blog"),
        "missing user var; env was:\n{stdout}"
    );
    assert!(
        stdout.contains("BOUGIE_SERVICE_MARIADB_PASSWORD="),
        "missing password var; env was:\n{stdout}"
    );

    stop_daemon(&env);
}
