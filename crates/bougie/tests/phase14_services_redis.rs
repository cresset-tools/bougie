//! Phase 14: end-to-end services-up/down/status with a fake-redis
//! fixture. Covers the contract `bougie services up redis` promises:
//!
//! - the daemon spawns the binary at the catalog tarball path,
//! - the unix socket the supervisor health-probes ends up bound,
//! - a tenant record lands in tenants.json with a redis DB number,
//! - a second project gets a distinct DB number,
//! - hitting the 16-tenant cap surfaces a `redis_db_exhausted` error,
//! - `services down` removes the tenant and stops the service when
//!   the last tenant goes away.
//!
//! Sandbox confinement isn't exercised here — that needs an actual
//! redis (`redis-cli SELECT` etc.). It's a follow-up integration test
//! once the bougie index can serve the redis-8.6.3 tarball.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::TestEnv;
use common::project_with_composer;
use std::fs;
use std::path::Path;
use std::time::Duration;

const STEP_TIMEOUT: Duration = Duration::from_secs(15);

/// Put the fake-redis binary at the catalog's expected store path so
/// the supervisor finds it.
fn install_fake_redis(env: &TestEnv) {
    use std::os::unix::fs::PermissionsExt;
    let store = env.home_path().join("store").join("redis-8.6.3").join("bin");
    fs::create_dir_all(&store).expect("mkdir store");
    let dst = store.join("redis-server");
    fs::copy(cargo_bin("fake-redis"), &dst).expect("copy fake-redis");
    // make it executable (cargo_bin already is, but make it explicit
    // in case the test env strips perms).
    let mut perms = fs::metadata(&dst).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&dst, perms).unwrap();
}

fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while !path.exists() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

fn stop_daemon(env: &TestEnv) {
    let _ = env
        .bougie()
        .args(["services", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert();
}

// -------------------- the happy path --------------------

#[test]
fn up_starts_fake_redis_and_provisions_a_tenant() {
    let env = TestEnv::new();
    install_fake_redis(&env);
    let proj = project_with_composer("acme/blog");

    env.bougie()
        .args(["services", "add", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    env.bougie()
        .args(["up", "--format", "json-v1"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Socket should exist.
    let sock = env
        .home_path()
        .join("state/services/redis/run/redis.sock");
    assert!(sock.exists(), "expected socket at {}", sock.display());

    // tenants.json should have one entry for our project with a db_number alloc.
    let tenants = env.home_path().join("state/services/redis/tenants.json");
    assert!(tenants.exists(), "tenants.json should exist");
    let ledger = fs::read_to_string(&tenants).unwrap();
    let line = ledger.lines().next().unwrap();
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["tenant"], "acme_blog");
    // macOS's `current_dir`-via-spawn resolves /var/folders → /private/var/folders;
    // the daemon stores the symlink-resolved path. Compare against the canonical
    // form on both sides so the assertion works on Linux and macOS alike.
    let expected = fs::canonicalize(proj.path()).unwrap();
    assert_eq!(v["project"], expected.to_str().unwrap());
    assert_eq!(v["alloc"]["db_number"], 0);

    stop_daemon(&env);
}

#[test]
fn daemon_stop_streams_per_service_drain_progress() {
    // With a service running, `daemon stop` drains it as part of the
    // shutdown and must surface that work: a `stopping redis` progress
    // line on stderr, and the stopped set in the terminal reply.
    let env = TestEnv::new();
    install_fake_redis(&env);
    let proj = project_with_composer("acme/blog");

    env.bougie()
        .args(["services", "add", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    let assertion = env
        .bougie()
        .args(["services", "daemon", "stop", "--format", "json-v1"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let out = assertion.get_output();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("stopping redis"),
        "expected per-service drain progress on stderr; got: {stderr}"
    );
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid JSON on stdout");
    assert_eq!(v["ok"], true);
    let stopped = v["stopped"].as_array().expect("stopped array");
    assert!(
        stopped.iter().any(|s| s == "redis"),
        "redis should be in the drained set; got {v}"
    );
    // The socket is gone synchronously — stop waited for full teardown.
    let sock = env.home_path().join("state").join("bougied.sock");
    assert!(!sock.exists(), "daemon socket should be gone after stop returns");
}

#[test]
fn status_after_up_reports_redis_running() {
    let env = TestEnv::new();
    install_fake_redis(&env);
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["services", "add", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let out = env
        .bougie()
        .args(["services", "status", "--format", "json-v1"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let services = v["services"].as_array().unwrap();
    assert_eq!(services.len(), 1, "{v}");
    assert_eq!(services[0]["name"], "redis");
    assert_eq!(services[0]["state"], "running");
    assert!(services[0]["pid"].as_u64().unwrap() > 0);
    stop_daemon(&env);
}

// -------------------- multi-project tenancy --------------------

#[test]
fn two_projects_share_one_redis_with_distinct_db_numbers() {
    let env = TestEnv::new();
    install_fake_redis(&env);
    let a = project_with_composer("acme/blog");
    let b = project_with_composer("acme/store");
    for p in [&a, &b] {
        env.bougie()
            .args(["services", "add", "redis"])
            .current_dir(p.path())
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
        env.bougie()
            .args(["up"])
            .current_dir(p.path())
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
    }
    let tenants_path = env.home_path().join("state/services/redis/tenants.json");
    let ledger = fs::read_to_string(&tenants_path).unwrap();
    let entries: Vec<serde_json::Value> = ledger
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(entries.len(), 2, "{ledger}");
    let db_numbers: Vec<u64> = entries
        .iter()
        .map(|e| e["alloc"]["db_number"].as_u64().unwrap())
        .collect();
    assert_eq!(db_numbers, vec![0, 1]);
    stop_daemon(&env);
}

#[test]
fn down_in_one_project_keeps_redis_running_for_the_other() {
    let env = TestEnv::new();
    install_fake_redis(&env);
    let a = project_with_composer("acme/blog");
    let b = project_with_composer("acme/store");
    for p in [&a, &b] {
        env.bougie()
            .args(["services", "add", "redis"])
            .current_dir(p.path())
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
        env.bougie()
            .args(["up"])
            .current_dir(p.path())
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
    }
    env.bougie()
        .args(["down"])
        .current_dir(a.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Socket still alive — b's tenant remains.
    let sock = env
        .home_path()
        .join("state/services/redis/run/redis.sock");
    assert!(sock.exists(), "socket must still exist after one project goes down");

    // Now drop b too; redis should stop.
    env.bougie()
        .args(["down"])
        .current_dir(b.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // The supervisor's stop() removes the child but the socket file
    // is left behind by the fake-redis fixture on SIGTERM (real redis
    // cleans up). What matters is that the daemon reports stopped.
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
    let services = v["services"].as_array().unwrap();
    let redis = services
        .iter()
        .find(|s| s["name"] == "redis")
        .expect("redis row");
    assert_eq!(redis["state"], "stopped");
    stop_daemon(&env);
}

// -------------------- redis_db_exhausted --------------------

#[test]
fn seventeenth_project_hits_redis_db_exhausted() {
    let env = TestEnv::new();
    install_fake_redis(&env);
    // 16 projects fit; the 17th must error.
    let mut projects = Vec::with_capacity(17);
    for i in 0..16 {
        let p = project_with_composer(&format!("acme/p{i}"));
        env.bougie()
            .args(["services", "add", "redis"])
            .current_dir(p.path())
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
        env.bougie()
            .args(["up"])
            .current_dir(p.path())
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
        projects.push(p);
    }
    let last = project_with_composer("acme/overflow");
    env.bougie()
        .args(["services", "add", "redis"])
        .current_dir(last.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let out = env
        .bougie()
        .args(["up"])
        .current_dir(last.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("16"), "{s}");
    assert!(s.contains("--purge"), "{s}");
    stop_daemon(&env);
}

// -------------------- idempotence --------------------

#[test]
fn up_is_idempotent_for_the_same_project() {
    let env = TestEnv::new();
    install_fake_redis(&env);
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["services", "add", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    for _ in 0..3 {
        env.bougie()
            .args(["up"])
            .current_dir(proj.path())
            .timeout(STEP_TIMEOUT)
            .assert()
            .success();
    }
    let tenants = env.home_path().join("state/services/redis/tenants.json");
    let entries: Vec<&str> = fs::read_to_string(&tenants)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .into_iter()
        .map(|l| Box::leak(l.to_string().into_boxed_str()) as &str)
        .collect();
    assert_eq!(entries.len(), 1, "{:?}", entries);
    stop_daemon(&env);
}

// -------------------- the auto-fetch error path --------------------

#[test]
fn up_with_no_tarball_falls_back_to_index_fetch() {
    // Without `install_fake_redis`, the daemon must reach for the
    // index to populate `store/redis-8.6.3/`. With BOUGIE_INDEX_URL
    // pointed at a non-routable address we don't actually fetch
    // anything; the test simply pins the contract that the
    // `service_tarball_fetch_failed` error surfaces a clear message
    // identifying the failing service.
    let env = TestEnv::new();
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["services", "add", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let out = env
        .bougie()
        // Loopback :1 is unbound on Linux runners; the connect
        // returns ECONNREFUSED in milliseconds.
        .env("BOUGIE_INDEX_URL", "http://127.0.0.1:1")
        .args(["up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("redis"), "{s}");
    // Match either the network-level failure surface or the
    // fetch-step wrapper, whichever phase trips first.
    assert!(
        s.contains("service_tarball_fetch_failed")
            || s.contains("tarball")
            || s.contains("connection")
            || s.contains("HTTP")
            || s.contains("Network"),
        "{s}"
    );
    stop_daemon(&env);
}

#[test]
fn projects_purge_removes_orphaned_redis_tenant() {
    let env = TestEnv::new();
    install_fake_redis(&env);
    let proj = project_with_composer("acme/blog");
    let proj_path = proj.path().to_path_buf();

    env.bougie()
        .args(["services", "add", "redis"])
        .current_dir(&proj_path)
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["up"])
        .current_dir(&proj_path)
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    let ledger = env.home_path().join("state/services/redis/tenants.json");
    assert!(
        fs::read_to_string(&ledger).unwrap().contains("acme_blog"),
        "tenant should be provisioned before purge"
    );

    // Orphan the project: dropping the TempDir deletes its directory.
    drop(proj);
    assert!(!proj_path.exists(), "project dir should be gone");

    // `purge` with no target picks the orphaned set. Runs from $HOME —
    // it reads the ledgers globally, no project cwd required.
    env.bougie()
        .args(["services", "projects", "purge", "--yes"])
        .current_dir(env.home_path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    let after = fs::read_to_string(&ledger).unwrap_or_default();
    assert!(
        !after.contains("acme_blog"),
        "orphaned tenant should be purged from the ledger: {after}"
    );

    stop_daemon(&env);
}

#[test]
fn projects_purge_refuses_noninteractive_without_yes() {
    // A scripted (non-TTY) purge without `--yes` must fail rather than
    // silently destroy data.
    let env = TestEnv::new();
    install_fake_redis(&env);
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["services", "add", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["up"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // `--all` without `--yes`, non-interactive → error, ledger intact.
    env.bougie()
        .args(["services", "projects", "purge", "--all"])
        .current_dir(env.home_path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .failure();
    let ledger = env.home_path().join("state/services/redis/tenants.json");
    assert!(
        fs::read_to_string(&ledger).unwrap().contains("acme_blog"),
        "ledger must be untouched when purge is refused"
    );
    stop_daemon(&env);
}

// Suppress dead-code warning on wait_for which is reserved for future
// log-rotation tests.
#[allow(dead_code)]
fn _ensure_wait_for_used() {
    let _ = wait_for;
}
