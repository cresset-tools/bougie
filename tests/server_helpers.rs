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

// `bougie server run` was a phase-0 placeholder; the live listener
// landed in phase 1 and is exercised by `tests/server_listener.rs`.

#[test]
fn manage_etc_hosts_auto_applies_on_add() {
    // Phase 5: when [server].manage_etc_hosts is true, every `bougie
    // server add` re-syncs the bougie sentinel block in /etc/hosts.
    // Integration tests target a tempfile via BOUGIE_ETC_HOSTS_PATH so
    // we don't need root.
    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let hosts_dir = TempDir::new().unwrap();
    let hosts_path = hosts_dir.path().join("hosts");
    std::fs::write(&hosts_path, "127.0.0.1 localhost\n").unwrap();

    // Pre-write server.toml with the flag on; `bougie server add`
    // mutates this file but preserves the [server] section.
    let cfg_dir = xdg.path().join("bougie");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("server.toml"),
        "[server]\nmanage_etc_hosts = true\n",
    )
    .unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("BOUGIE_ETC_HOSTS_PATH", &hosts_path)
        .args([
            "server",
            "add",
            "myapp.bougie.test",
            proj.path().to_str().unwrap(),
            "--root",
            "public",
        ])
        .assert()
        .success()
        .stdout(contains("added myapp.bougie.test"));

    let body = std::fs::read_to_string(&hosts_path).unwrap();
    assert!(body.contains("127.0.0.1 localhost"), "preserved: {body}");
    assert!(body.contains("# BEGIN bougie"), "block added: {body}");
    assert!(body.contains("127.0.0.1 myapp.bougie.test"), "v4 entry: {body}");
    assert!(body.contains("::1 myapp.bougie.test"), "v6 entry: {body}");
    assert!(body.contains("# END bougie"));

    // remove should re-sync, dropping the block.
    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("BOUGIE_ETC_HOSTS_PATH", &hosts_path)
        .args(["server", "remove", "myapp.bougie.test"])
        .assert()
        .success();
    let body = std::fs::read_to_string(&hosts_path).unwrap();
    assert!(body.contains("127.0.0.1 localhost"));
    assert!(!body.contains("myapp.bougie.test"));
    assert!(!body.contains("# BEGIN bougie"));
}

#[test]
fn manage_etc_hosts_off_does_not_touch_hosts_file() {
    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let hosts_dir = TempDir::new().unwrap();
    let hosts_path = hosts_dir.path().join("hosts");
    let initial = "127.0.0.1 localhost\n";
    std::fs::write(&hosts_path, initial).unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("BOUGIE_ETC_HOSTS_PATH", &hosts_path)
        .args([
            "server",
            "add",
            "no-auto.bougie.run",
            proj.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let body = std::fs::read_to_string(&hosts_path).unwrap();
    assert_eq!(body, initial, "hosts file untouched when flag is off");
}

#[test]
fn hosts_apply_with_tempfile_target() {
    // Stand-alone test for `bougie server hosts apply` against the
    // tempfile path. Independent of the auto-add path so we can lean
    // on it in case the helpers refactor.
    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let hosts_dir = TempDir::new().unwrap();
    let hosts_path = hosts_dir.path().join("hosts");
    std::fs::write(&hosts_path, "127.0.0.1 localhost\n").unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args([
            "server",
            "add",
            "explicit.bougie.test",
            proj.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("BOUGIE_ETC_HOSTS_PATH", &hosts_path)
        .args(["server", "hosts", "apply"])
        .assert()
        .success()
        .stdout(contains("synced"));

    let body = std::fs::read_to_string(&hosts_path).unwrap();
    assert!(body.contains("explicit.bougie.test"));
}
