//! Integration tests for `bougie server add/remove/list` (phase 0).
//! `run` is exercised only to confirm it errors out cleanly until the
//! phase-1 implementation lands.

mod common;

use common::TestEnv;
use predicates::str::contains;
use tempfile::TempDir;

fn server_toml_path(xdg_config: &std::path::Path) -> std::path::PathBuf {
    xdg_config.join("bougie").join("server.toml")
}

#[test]
fn add_then_list_then_remove() {
    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args([
            "server",
            "add",
            "myapp.bougie.run",
            proj.path().to_str().unwrap(),
            "--root",
            "public",
        ])
        .assert()
        .success()
        .stdout(contains("added myapp.bougie.run"));

    let cfg_path = server_toml_path(xdg.path());
    let body = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(body.contains("myapp.bougie.run"));
    assert!(body.contains("public"));

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["server", "list"])
        .assert()
        .success()
        .stdout(contains("myapp.bougie.run"))
        .stdout(contains("root=public"));

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["server", "remove", "myapp.bougie.run"])
        .assert()
        .success()
        .stdout(contains("removed myapp.bougie.run"));

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["server", "list"])
        .assert()
        .success()
        .stdout(contains("no hosts configured"));
}

#[test]
fn list_json_v1_has_schema_and_hosts() {
    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["server", "add", "a.bougie.run", proj.path().to_str().unwrap()])
        .assert()
        .success();

    let out = env
        .bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["--format", "json-v1", "server", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["hosts"][0]["hostname"], "a.bougie.run");
    assert_eq!(parsed["hosts"][0]["root"], ".");
}

#[test]
fn add_is_idempotent() {
    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["server", "add", "x.bougie.run", proj.path().to_str().unwrap()])
        .assert()
        .success();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["server", "add", "x.bougie.run", proj.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(contains("already configured"));
}

#[test]
fn remove_missing_host_exits_nonzero() {
    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["server", "remove", "ghost.bougie.run"])
        .assert()
        .failure()
        .stdout(contains("no host ghost.bougie.run"));
}

#[test]
fn add_rejects_bad_hostname() {
    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args([
            "server",
            "add",
            "--",
            "under_score.bougie.run",
            proj.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("hostname"));
}

#[test]
fn run_placeholder_errors_actionably() {
    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["server", "run"])
        .assert()
        .failure()
        .stderr(contains("not implemented yet"))
        .stderr(contains("phase 1"));
}
