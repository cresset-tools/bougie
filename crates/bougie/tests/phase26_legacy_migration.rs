//! Phase 5: **legacy → version-keyed migration**, end to end against a
//! *real* mariadb.
//!
//! Existing users upgrading into the multi-instance world have a
//! pre-version-keying state tree (`state/services/mariadb/{data,
//! tenants.json,…}`, no `<version>/` segment) and an `app/etc/env.php`
//! with the old flat socket path baked in. This test proves the upgrade
//! is non-destructive and self-healing:
//!
//!   1. bring mariadb up on the current (version-keyed) layout, provision
//!      a tenant, and write a **sentinel row** into its database;
//!   2. flatten the state back to the legacy shape and bake the old flat
//!      socket path into a Magento-style `env.php` — i.e. reconstruct
//!      exactly what a shipped-bougie install looks like on disk;
//!   3. `bougie up` again — the new daemon runs `migrate_legacy_service_state`.
//!
//! Then assert the whole upgrade contract holds:
//!   - the datadir is *migrated* (renamed) under `<version>/`, not
//!     reinitialized — the sentinel row and the original password both
//!     survive (a fresh datadir would have neither);
//!   - the project's stable connection socket symlink is (re)created and
//!     resolves to the live instance;
//!   - the stale flat socket path in `env.php` is rewritten to that
//!     stable socket.
//!
//! Skipped under `BOUGIE_SKIP_REAL_MARIADB=1`.

mod common;

use common::mariadb_fixture;
use common::project_with_composer;
use common::TestEnv;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const MARIADB_VERSION: &str = "11.4.4";
const STEP_TIMEOUT: Duration = Duration::from_mins(2);

fn should_skip() -> bool {
    std::env::var_os("BOUGIE_SKIP_REAL_MARIADB").is_some()
}

fn stop_daemon(env: &TestEnv) {
    let _ = env
        .bougie()
        .args(["service", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert();
    // Let the daemon SIGTERM the mariadbd child and release the datadir
    // lock files before we shuffle the state tree.
    std::thread::sleep(Duration::from_millis(600));
}

fn mariadb_client(env: &TestEnv) -> std::path::PathBuf {
    env.home_path()
        .join("store")
        .join(mariadb_fixture::MARIADB_TARBALL)
        .join("bin/mariadb")
}

/// The instance's real bound socket (`state/run/<token>/mariadb.sock`).
fn instance_socket(env: &TestEnv) -> std::path::PathBuf {
    env.home_path()
        .join("state/run")
        .join(bougie_paths::instance_run_token("mariadb", MARIADB_VERSION))
        .join("mariadb.sock")
}

fn versioned_dir(env: &TestEnv) -> std::path::PathBuf {
    env.home_path().join("state/services/mariadb").join(MARIADB_VERSION)
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

/// Run one SQL statement as the tenant over its socket; returns trimmed stdout.
fn run_sql(client: &Path, sock: &Path, user: &str, pw: &str, db: &str, sql: &str) -> std::process::Output {
    Command::new(client)
        .arg("--no-defaults")
        .arg(format!("--socket={}", sock.display()))
        .arg(format!("--user={user}"))
        .arg(format!("--password={pw}"))
        .arg(format!("--database={db}"))
        .arg("--batch")
        .arg("--skip-column-names")
        .arg("-e")
        .arg(sql)
        .output()
        .expect("running mariadb client")
}

fn up(env: &TestEnv, proj: &Path) {
    env.bougie()
        .args(["service", "up", "--format", "json-v1"])
        .current_dir(proj)
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
}

#[test]
fn legacy_flat_install_upgrades_without_data_loss() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_MARIADB set");
        return;
    }
    let env = TestEnv::new();
    mariadb_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");
    let client = mariadb_client(&env);

    // --- 1. Bring mariadb up on the current layout and seed a sentinel. ---
    env.bougie()
        .args(["service", "add", "mariadb"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    up(&env, proj.path());
    assert!(
        wait_for(&instance_socket(&env), Duration::from_secs(60)),
        "mariadb socket never appeared"
    );

    // Read the provisioned tenant (db == user == tenant name) + password.
    let ledger = versioned_dir(&env).join("tenants.json");
    let row: serde_json::Value = serde_json::from_str(
        fs::read_to_string(&ledger).expect("tenants.json").lines().next().unwrap(),
    )
    .unwrap();
    let tenant = row["tenant"].as_str().unwrap().to_string();
    let password = row["secrets"]["password"].as_str().unwrap().to_string();

    // Write a sentinel row into the tenant DB — the proof that the datadir
    // is migrated, not reinitialized.
    let out = run_sql(
        &client,
        &instance_socket(&env),
        &tenant,
        &password,
        &tenant,
        "CREATE TABLE migration_probe (v INT); INSERT INTO migration_probe VALUES (4242);",
    );
    assert!(out.status.success(), "seed SQL failed: {}", String::from_utf8_lossy(&out.stderr));

    // A Magento-style env.php with the OLD flat socket path baked in.
    let flat_socket = env.home_path().join("state/services/mariadb/run/mariadb.sock");
    let env_php_dir = proj.path().join("app/etc");
    fs::create_dir_all(&env_php_dir).unwrap();
    let env_php = env_php_dir.join("env.php");
    fs::write(
        &env_php,
        format!(
            "<?php\nreturn array (\n  'db' => array ( 'connection' => array ( 'default' => array (\n\
             \x20   'host' => '{}',\n    'username' => '{tenant}',\n    'password' => '{password}',\n\
             \x20   'dbname' => '{tenant}',\n  ) ) ),\n);\n",
            flat_socket.display()
        ),
    )
    .unwrap();

    stop_daemon(&env);

    // --- 2. Flatten the state tree to the pre-version-keying shape. ---
    // Move <version>/{data,tenants.json,conf,log} up to <name>/ and drop
    // the version dir + the new-layout run/conn dirs a legacy install
    // never had, so the daemon sees a genuine legacy marker.
    let name_dir = env.home_path().join("state/services/mariadb");
    let versioned = versioned_dir(&env);
    for entry in fs::read_dir(&versioned).unwrap() {
        let e = entry.unwrap();
        fs::rename(e.path(), name_dir.join(e.file_name())).unwrap();
    }
    fs::remove_dir(&versioned).unwrap();
    let _ = fs::remove_dir_all(env.home_path().join("state/run"));
    let _ = fs::remove_dir_all(env.home_path().join("state/conn"));
    assert!(
        name_dir.join("data").is_dir() && name_dir.join("tenants.json").is_file(),
        "flatten should leave a legacy marker (data/ + tenants.json under <name>/)"
    );
    assert!(!versioned.exists(), "the version dir must be gone before the upgrade");

    // --- 3. Upgrade: `bougie up` migrates, boots, relinks, rewrites. ---
    up(&env, proj.path());
    assert!(
        wait_for(&instance_socket(&env), Duration::from_secs(60)),
        "mariadb socket never reappeared after migration"
    );

    // Datadir was migrated (renamed) under <version>/, not reinitialized.
    assert!(versioned_dir(&env).join("data").is_dir(), "datadir must be back under <version>/");

    // The sentinel row survived — proves the *original* datadir migrated,
    // and the login proves the original password migrated with it (a fresh
    // datadir would have neither the row nor this user).
    let out = run_sql(
        &client,
        &instance_socket(&env),
        &tenant,
        &password,
        &tenant,
        "SELECT v FROM migration_probe;",
    );
    assert!(
        out.status.success(),
        "post-migration login/query failed — datadir may have been reinitialized: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        "4242",
        "sentinel row must survive the migration"
    );

    // The project's stable conn socket is (re)created and resolves to the
    // live instance.
    let conn = env
        .home_path()
        .join("state/conn")
        .join(bougie_paths::project_hash(proj.path()))
        .join("mariadb.sock");
    assert_eq!(
        fs::read_link(&conn).ok(),
        Some(instance_socket(&env)),
        "conn socket must resolve to the migrated instance"
    );

    // The stale flat socket path in env.php was rewritten to the stable one.
    let env_php_after = fs::read_to_string(&env_php).unwrap();
    assert!(
        env_php_after.contains(&conn.display().to_string()),
        "env.php should now point at the stable conn socket:\n{env_php_after}"
    );
    assert!(
        !env_php_after.contains(&flat_socket.display().to_string()),
        "the old flat socket path must be gone from env.php:\n{env_php_after}"
    );

    stop_daemon(&env);
}
