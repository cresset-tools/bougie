//! Phase 13: offline `bougie service {catalog,add,remove,list}` —
//! the subcommands that don't need a running `bougied`.

mod common;

use common::TestEnv;
use std::fs;
use tempfile::TempDir;

const STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Construct an empty project dir containing just `{}` in composer.json.
fn empty_project() -> TempDir {
    let dir = TempDir::new().expect("tempdir for project");
    fs::write(dir.path().join("composer.json"), "{}\n").unwrap();
    dir
}

// -------------------- catalog --------------------

#[test]
fn catalog_text_lists_user_facing_only() {
    let env = TestEnv::new();
    let out = env
        .bougie()
        .args(["service", "catalog"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("redis"), "missing redis: {s}");
    assert!(s.contains("mariadb"), "missing mariadb: {s}");
    assert!(s.contains("opensearch"), "missing opensearch: {s}");
    assert!(s.contains("rabbitmq"), "missing rabbitmq: {s}");
    assert!(s.contains("server"), "missing server: {s}");
    // jdk + erlang are runtime-only deps and should NOT appear.
    assert!(!s.contains("jdk"), "unexpected jdk in user-facing output: {s}");
    assert!(!s.contains("erlang"), "unexpected erlang in user-facing output: {s}");
}

#[test]
fn catalog_json_v1_contains_every_entry() {
    let env = TestEnv::new();
    let out = env
        .bougie()
        .args(["service", "catalog", "--format", "json-v1"])
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["schema_version"], 1);
    let entries = v["entries"].as_array().expect("entries array");
    let names: Vec<_> = entries
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(names.contains(&"redis"));
    assert!(names.contains(&"jdk"), "JSON output exposes runtime deps too: {names:?}");
    assert!(names.contains(&"erlang"));

    // Every entry carries a non-empty `versions` list whose first element is
    // the default `version` (additive to schema 1 — back-compat preserved).
    for e in entries {
        let name = e["name"].as_str().unwrap();
        let versions = e["versions"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} missing versions array: {e}"));
        assert!(!versions.is_empty(), "{name} has an empty versions list");
        assert_eq!(
            versions[0], e["version"],
            "{name}: versions[0] should be the default version"
        );
    }

    // mysql is the multi-version service — the catalog offers 8.4 *and* 8.0.
    let mysql = entries
        .iter()
        .find(|e| e["name"] == "mysql")
        .expect("mysql entry");
    let mysql_versions = mysql["versions"].as_array().unwrap();
    assert!(
        mysql_versions.len() >= 2,
        "mysql should advertise multiple versions, got {mysql_versions:?}"
    );
}

// -------------------- add --------------------

#[test]
fn add_bare_name_writes_star_pin_to_composer_json() {
    let env = TestEnv::new();
    let proj = empty_project();
    env.bougie()
        .args(["service", "add", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let composer: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(proj.path().join("composer.json")).unwrap())
            .unwrap();
    assert_eq!(composer["extra"]["bougie"]["services"]["redis"], "*");
}

#[test]
fn add_with_version_pin_writes_exact_version() {
    let env = TestEnv::new();
    let proj = empty_project();
    env.bougie()
        .args(["service", "add", "mariadb@11.4"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let composer: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(proj.path().join("composer.json")).unwrap())
            .unwrap();
    assert_eq!(composer["extra"]["bougie"]["services"]["mariadb"], "11.4");
}

#[test]
fn add_is_idempotent_at_same_pin() {
    let env = TestEnv::new();
    let proj = empty_project();
    env.bougie()
        .args(["service", "add", "redis@8.6"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let out = env
        .bougie()
        .args(["service", "add", "redis@8.6", "--format", "json-v1"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["items"][0]["already_present"], true);
}

#[test]
fn add_unknown_service_errors_with_known_list() {
    let env = TestEnv::new();
    let proj = empty_project();
    let out = env
        .bougie()
        .args(["service", "add", "postgres"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("unknown service"), "{s}");
    assert!(s.contains("redis"), "{s}");
    assert!(s.contains("mariadb"), "{s}");
}

#[test]
fn add_mysql_is_rejected_when_mariadb_already_declared() {
    // A project runs one relational DB. mariadb is declared; adding mysql
    // must fail before touching composer.json.
    let env = TestEnv::new();
    let proj = empty_project();
    env.bougie()
        .args(["service", "add", "mariadb"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let out = env
        .bougie()
        .args(["service", "add", "mysql"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("mutually exclusive"), "{s}");
    // composer.json still declares only mariadb — the write was refused.
    let composer = fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert!(composer.contains("mariadb"), "{composer}");
    assert!(!composer.contains("mysql"), "mysql should not have been written: {composer}");
}

#[test]
fn add_mariadb_and_mysql_together_is_rejected() {
    // Both in one invocation → rejected, nothing written.
    let env = TestEnv::new();
    let proj = empty_project();
    let out = env
        .bougie()
        .args(["service", "add", "mariadb", "mysql"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8(out).unwrap().contains("mutually exclusive"));
    let composer = fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert!(!composer.contains("mariadb") && !composer.contains("mysql"), "{composer}");
}

#[test]
fn add_runtime_dep_is_rejected() {
    let env = TestEnv::new();
    let proj = empty_project();
    let out = env
        .bougie()
        .args(["service", "add", "jdk"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8(out).unwrap().contains("runtime dep"));
}

#[test]
fn add_targets_bougie_toml_when_it_exists() {
    let env = TestEnv::new();
    let proj = empty_project();
    fs::write(proj.path().join("bougie.toml"), "[php]\n").unwrap();
    env.bougie()
        .args(["service", "add", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    // composer.json should be untouched.
    let composer = fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert!(
        !composer.contains("services"),
        "composer.json must not be mutated when bougie.toml exists: {composer}"
    );
    let toml = fs::read_to_string(proj.path().join("bougie.toml")).unwrap();
    assert!(toml.contains("[services]"), "expected [services] in bougie.toml: {toml}");
    assert!(toml.contains("redis = \"*\""), "{toml}");
}

// -------------------- remove --------------------

#[test]
fn remove_drops_entry_from_composer_json() {
    let env = TestEnv::new();
    let proj = empty_project();
    env.bougie()
        .args(["service", "add", "redis", "mariadb"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    env.bougie()
        .args(["service", "remove", "redis"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let composer: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(proj.path().join("composer.json")).unwrap())
            .unwrap();
    let services = &composer["extra"]["bougie"]["services"];
    assert!(services["redis"].is_null(), "redis should be gone");
    assert_eq!(services["mariadb"], "*");
}

#[test]
fn remove_absent_service_reports_not_declared_but_succeeds() {
    let env = TestEnv::new();
    let proj = empty_project();
    let out = env
        .bougie()
        .args(["service", "remove", "redis", "--format", "json-v1"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["items"][0]["removed"], false);
}

// -------------------- list --------------------

#[test]
fn list_empty_project_reports_nothing_declared() {
    let env = TestEnv::new();
    let proj = empty_project();
    let out = env
        .bougie()
        .args(["service", "list"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("no services declared"), "{s}");
}

#[test]
fn list_after_add_shows_declared_services_alphabetised() {
    let env = TestEnv::new();
    let proj = empty_project();
    env.bougie()
        .args(["service", "add", "redis", "mariadb@11.4"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    let out = env
        .bougie()
        .args(["service", "list", "--format", "json-v1"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let services = v["services"].as_array().unwrap();
    assert_eq!(services.len(), 2);
    assert_eq!(services[0]["name"], "mariadb"); // alpha order
    assert_eq!(services[0]["version"], "11.4");
    assert_eq!(services[1]["name"], "redis");
    assert_eq!(services[1]["version"], "*");
}

#[test]
fn list_reads_both_composer_json_and_bougie_toml_merged() {
    let env = TestEnv::new();
    let proj = empty_project();
    // Put one service in composer.json, another in bougie.toml.
    fs::write(
        proj.path().join("composer.json"),
        r#"{"extra":{"bougie":{"services":{"redis":"8.6"}}}}"#,
    )
    .unwrap();
    fs::write(
        proj.path().join("bougie.toml"),
        "[services]\nmariadb = \"11.4\"\n",
    )
    .unwrap();
    let out = env
        .bougie()
        .args(["service", "list", "--format", "json-v1"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let services = v["services"].as_array().unwrap();
    assert_eq!(services.len(), 2, "{v}");
}
