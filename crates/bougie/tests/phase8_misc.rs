//! Phase 8 sanity: trivial commands that should work without an index.

mod common;

use common::TestEnv;
use predicates::str::contains;

#[test]
fn cache_clean_succeeds_when_cache_is_empty() {
    let env = TestEnv::new();
    env.bougie()
        .args(["cache", "clean"])
        .assert()
        .success()
        .stdout(contains("wiped"));
}

#[test]
fn cache_size_reports_zero_for_empty_env() {
    let env = TestEnv::new();
    env.bougie()
        .args(["cache", "size"])
        .assert()
        .success()
        .stdout(contains("total"))
        .stdout(contains("0 B"));
}

#[test]
fn ext_list_reads_composer_extensions() {
    // The renderer strips the `ext-` prefix to match how every other
    // bougie surface refers to extensions (`bougie ext add xdebug`,
    // not `ext-xdebug`).
    let env = TestEnv::new();
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"require":{"php":"^8.3","ext-xdebug":"*","ext-redis":"*"}}"#,
    )
    .unwrap();
    env.bougie()
        .current_dir(proj.path())
        // --only-installed avoids the network fetch this offline test
        // can't satisfy.
        .args(["ext", "list", "--only-installed"])
        .assert()
        .success()
        .stdout(contains("xdebug"))
        .stdout(contains("redis"))
        .stdout(contains("required"));
}

#[test]
fn ext_add_outside_a_project_fails_actionably() {
    let env = TestEnv::new();
    let proj = tempfile::TempDir::new().unwrap();
    env.bougie()
        .current_dir(proj.path())
        .args(["ext", "add", "xdebug"])
        .assert()
        .failure()
        .stderr(contains("no bougie project here"));
}

#[test]
fn ext_add_in_project_without_sync_triggers_implicit_sync() {
    // The shim is missing → ext add should attempt to sync. Without
    // network this still fails, but the failure must be the underlying
    // sync error, not a "run `bougie sync` first" handoff. The
    // user-visible signal that the implicit sync ran is the
    // "Syncing…" line on stderr.
    let env = TestEnv::new();
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join("vendor").join("bougie")).unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"require":{"php":"8.3.12"}}"#,
    )
    .unwrap();
    env.bougie()
        .current_dir(proj.path())
        // Point the index at an unreachable URL so sync fails fast,
        // proving we attempted it rather than handing off.
        .env("BOUGIE_INDEX_URL", "http://127.0.0.1:1")
        .args(["ext", "add", "xdebug"])
        .assert()
        .failure()
        .stderr(contains("Syncing…"));
}

#[test]
fn php_pin_writes_to_bougie_toml() {
    let env = TestEnv::new();
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(proj.path().join("bougie.toml"), "").unwrap();
    env.bougie()
        .current_dir(proj.path())
        .args(["php", "pin", "8.3.12"])
        .assert()
        .success();
    let body = std::fs::read_to_string(proj.path().join("bougie.toml")).unwrap();
    assert!(body.contains("8.3.12"), "got: {body}");
    assert!(body.contains("[php]"), "got: {body}");
}

#[test]
fn php_pin_writes_to_composer_extra_when_no_toml() {
    let env = TestEnv::new();
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"require":{"php":"^8.3"}}"#,
    )
    .unwrap();
    env.bougie()
        .current_dir(proj.path())
        .args(["php", "pin", "8.3.12"])
        .assert()
        .success();
    let body = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["extra"]["bougie"]["php"]["version"], "8.3.12");
}

#[test]
fn self_update_honors_no_self_update_env() {
    // `BOUGIE_NO_SELF_UPDATE=1` is the escape hatch for environments
    // where an external installer owns the binary (package manager,
    // nix store, …). The updater must refuse cleanly without touching
    // the network. Asserting the env-var path keeps this test
    // deterministic — the real download flow is covered by the unit
    // tests in `commands::self_update::tests`.
    let env = TestEnv::new();
    env.bougie()
        .env("BOUGIE_NO_SELF_UPDATE", "1")
        .args(["self", "update"])
        .assert()
        .failure()
        .stderr(contains("BOUGIE_NO_SELF_UPDATE"));
}
