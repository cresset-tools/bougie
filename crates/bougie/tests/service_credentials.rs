//! `bougie service credentials` — offline tenant connection info.
//!
//! No daemon: the command reads the on-disk tenant ledgers
//! (`$BOUGIE_HOME/state/services/<svc>/tenants.json`) directly, so the
//! tests plant ledger rows by hand and assert on the three output
//! forms (text, json-v1, --env).

mod common;

use common::TestEnv;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

const STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Project dir with mariadb + redis declared (as `bougie service add`
/// writes them).
fn project_with_services() -> TempDir {
    let dir = TempDir::new().expect("tempdir for project");
    fs::write(
        dir.path().join("composer.json"),
        r#"{"extra":{"bougie":{"services":{"mariadb":"*","redis":"*"}}}}"#,
    )
    .unwrap();
    dir
}

/// Append a mariadb tenant row for `project`, the way the provisioner
/// records it (password persisted in `secrets`).
fn plant_mariadb_tenant(env: &TestEnv, project: &Path, password: &str) {
    let dir = env.home_path().join("state/services/mariadb");
    fs::create_dir_all(&dir).unwrap();
    let canon = project.canonicalize().unwrap();
    let row = serde_json::json!({
        "schema_version": 1,
        "tenant": "acme",
        "project": canon,
        "created_at": "2026-07-01T00:00:00Z",
        "secrets": {"password": password},
        "alloc": {},
    });
    fs::write(dir.join("tenants.json"), format!("{row}\n")).unwrap();
}

#[test]
fn text_shows_password_and_not_provisioned_note() {
    let env = TestEnv::new();
    let proj = project_with_services();
    plant_mariadb_tenant(&env, proj.path(), "s3cret48hex");

    let out = env
        .bougie()
        .args(["service", "credentials"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("mariadb (tenant acme)"), "{s}");
    assert!(s.contains("password"), "{s}");
    assert!(s.contains("s3cret48hex"), "{s}");
    assert!(s.contains("database"), "{s}");
    assert!(s.contains("mariadb.sock"), "{s}");
    // redis is declared but has no tenant row.
    assert!(
        s.contains("redis: not provisioned — run `bougie up redis` first"),
        "{s}"
    );
}

#[test]
fn json_v1_carries_connection_map() {
    let env = TestEnv::new();
    let proj = project_with_services();
    plant_mariadb_tenant(&env, proj.path(), "s3cret48hex");

    let out = env
        .bougie()
        .args(["service", "credentials", "mariadb", "--format", "json-v1"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["schema_version"], 1);
    let svc = &v["services"][0];
    assert_eq!(svc["service"], "mariadb");
    assert_eq!(svc["tenant"], "acme");
    assert_eq!(svc["connection"]["password"], "s3cret48hex");
    assert_eq!(svc["connection"]["database"], "acme");
    assert_eq!(svc["connection"]["user"], "acme");
    assert!(
        svc["connection"]["socket"]
            .as_str()
            .unwrap()
            .ends_with("mariadb.sock")
    );
}

#[test]
fn env_emits_the_bougie_run_variable_names() {
    let env = TestEnv::new();
    let proj = project_with_services();
    plant_mariadb_tenant(&env, proj.path(), "s3cret48hex");

    let out = env
        .bougie()
        .args(["service", "credentials", "--env"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(
        s.contains("BOUGIE_SERVICE_MARIADB_PASSWORD='s3cret48hex'"),
        "{s}"
    );
    assert!(s.contains("BOUGIE_SERVICE_MARIADB_DATABASE='acme'"), "{s}");
    // Not-provisioned services surface as eval-safe comments.
    assert!(s.contains("# redis: not provisioned"), "{s}");
}

#[test]
fn ledger_row_without_password_falls_back_to_derivation() {
    let env = TestEnv::new();
    let proj = project_with_services();
    // Pre-derivation ledger row: no secrets recorded.
    let dir = env.home_path().join("state/services/mariadb");
    fs::create_dir_all(&dir).unwrap();
    let canon = proj.path().canonicalize().unwrap();
    let row = serde_json::json!({
        "schema_version": 1,
        "tenant": "acme",
        "project": canon,
        "created_at": "2026-07-01T00:00:00Z",
    });
    fs::write(dir.join("tenants.json"), format!("{row}\n")).unwrap();

    let password_of = |env: &TestEnv| -> String {
        let out = env
            .bougie()
            .args(["service", "credentials", "mariadb", "--format", "json-v1"])
            .current_dir(proj.path())
            .timeout(STEP_TIMEOUT)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        v["services"][0]["connection"]["password"]
            .as_str()
            .expect("derived password present")
            .to_string()
    };

    let first = password_of(&env);
    assert_eq!(first.len(), 48, "derive_password format: {first}");
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first}");
    // Stable across invocations (keyed on machine secret + project).
    assert_eq!(first, password_of(&env));
}

#[test]
fn explicit_name_without_tenant_errors_with_up_hint() {
    let env = TestEnv::new();
    let proj = project_with_services();

    let out = env
        .bougie()
        .args(["service", "credentials", "mariadb"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("no mariadb tenant is provisioned"), "{s}");
    assert!(s.contains("bougie up mariadb"), "{s}");
}

#[test]
fn unknown_service_lists_known_names() {
    let env = TestEnv::new();
    let proj = project_with_services();

    let out = env
        .bougie()
        .args(["service", "credentials", "nope"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("unknown service `nope`"), "{s}");
    assert!(s.contains("mariadb"), "{s}");
}
