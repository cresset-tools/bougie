//! Phase 3: `bougie init` and `bougie init --toml`.

mod common;

use common::TestEnv;
use predicates::str::contains;
use tempfile::TempDir;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn project_dir() -> TempDir {
    TempDir::new().expect("project tempdir")
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn init_creates_composer_and_bougie_skeleton() {
    let env = TestEnv::new();
    let proj = project_dir();
    env.bougie()
        .current_dir(proj.path())
        .arg("init")
        .assert()
        .success()
        .stdout(contains("composer.json"))
        .stdout(contains(".bougie/conf.d"));

    assert!(proj.path().join("composer.json").is_file());
    assert!(proj.path().join(".bougie/conf.d").is_dir());
    assert!(proj.path().join(".bougie/bin").is_dir());
    assert!(proj.path().join(".bougie/state").is_dir());
    assert!(proj.path().join(".bougie/.gitignore").is_file());
    assert!(!proj.path().join("bougie.toml").exists());
    assert!(!proj.path().join(".php-version").exists());
}

#[test]
fn init_name_sets_composer_name() {
    let env = TestEnv::new();
    let proj = project_dir();
    env.bougie()
        .current_dir(proj.path())
        .args(["init", "--name", "acme/widget"])
        .assert()
        .success();

    let composer: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(proj.path().join("composer.json")).unwrap())
            .unwrap();
    assert_eq!(composer["name"], "acme/widget");
    assert!(composer["require"]["php"].is_string());
}

#[test]
fn init_rejects_invalid_name() {
    let env = TestEnv::new();
    let proj = project_dir();
    env.bougie()
        .current_dir(proj.path())
        .args(["init", "--name", "NotAValidName"])
        .assert()
        .failure()
        .stderr(contains("invalid package name"));

    assert!(!proj.path().join("composer.json").exists());
}

#[test]
fn init_does_not_overwrite_existing_composer() {
    let env = TestEnv::new();
    let proj = project_dir();
    let custom = r#"{"name":"acme/example","require":{"php":"^8.2"}}"#;
    std::fs::write(proj.path().join("composer.json"), custom).unwrap();

    env.bougie()
        .current_dir(proj.path())
        .arg("init")
        .assert()
        .success()
        .stdout(contains("kept"));

    let read = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert_eq!(read, custom);
}

#[test]
fn init_toml_creates_bougie_toml() {
    let env = TestEnv::new();
    let proj = project_dir();
    env.bougie()
        .current_dir(proj.path())
        .args(["init", "--toml"])
        .assert()
        .success();
    let toml = proj.path().join("bougie.toml");
    assert!(toml.is_file());
    let text = std::fs::read_to_string(&toml).unwrap();
    assert!(text.contains("[php]"));
    assert!(text.contains("[extensions]"));
}

#[test]
fn init_default_composer_has_require_php() {
    let env = TestEnv::new();
    let proj = project_dir();
    env.bougie()
        .current_dir(proj.path())
        .arg("init")
        .assert()
        .success();
    let text = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let php = v["require"]["php"].as_str().unwrap();
    assert!(php.starts_with('^'));
}

#[test]
fn init_json_v1_envelope() {
    let env = TestEnv::new();
    let proj = project_dir();
    let out = env
        .bougie()
        .current_dir(proj.path())
        .args(["init", "--format", "json-v1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert!(v["created"].is_array());
}

#[test]
fn init_is_idempotent() {
    let env = TestEnv::new();
    let proj = project_dir();
    env.bougie().current_dir(proj.path()).arg("init").assert().success();
    let first = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();

    env.bougie()
        .current_dir(proj.path())
        .arg("init")
        .assert()
        .success()
        .stdout(contains("kept"));
    let second = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert_eq!(first, second);
}

#[test]
fn init_starter_writes_manifest_composer_json() {
    let env = TestEnv::new();
    let proj = project_dir();
    let manifest = r#"{
        "schema": 1,
        "name": "Mage-OS test starter",
        "composer-json": {
            "name": "acme/from-starter",
            "require": {"php": "^8.4", "mage-os/product-community-edition": "^3.0"}
        },
        "services": ["mariadb", "redis"],
        "recipe": "magento",
        "notes": ["Hyvä themes need a license token"]
    }"#;

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/starter.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(manifest))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    env.bougie()
        .current_dir(proj.path())
        .args(["init", "--starter"])
        .arg(format!("{uri}/starter.json"))
        .assert()
        .success()
        .stdout(contains("composer.json"))
        .stderr(contains("note: Hyvä themes need a license token"));

    // composer.json came from the starter, not the empty default.
    let cj = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert!(cj.contains("acme/from-starter"), "{cj}");
    assert!(cj.contains("mage-os/product-community-edition"), "{cj}");
    // Normal scaffolding still happened.
    assert!(proj.path().join(".bougie/conf.d").is_dir());
}

#[test]
fn init_starter_appends_starter_json_to_a_base_url() {
    // A `--starter` value that doesn't end in `.json` is treated as a
    // starter *base* (e.g. the maker's `…/c/{id}` share link, which is an
    // HTML page): bougie fetches `<base>/starter.json`.
    let env = TestEnv::new();
    let proj = project_dir();
    let manifest = r#"{"schema":1,"composer-json":{"name":"acme/from-base","require":{"php":"^8.4"}}}"#;

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/c/abc-123/starter.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(manifest))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    env.bougie()
        .current_dir(proj.path())
        .args(["init", "--starter"])
        .arg(format!("{uri}/c/abc-123"))
        .assert()
        .success();

    let cj = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert!(cj.contains("acme/from-base"), "{cj}");
}

#[test]
fn init_starter_refuses_existing_composer() {
    let env = TestEnv::new();
    let proj = project_dir();
    std::fs::write(proj.path().join("composer.json"), "{}").unwrap();

    // The existing-project guard fires before any fetch, so the bogus
    // URL is never contacted.
    env.bougie()
        .current_dir(proj.path())
        .args(["init", "--starter", "https://example.invalid/x.json"])
        .assert()
        .failure()
        .stderr(contains("already exists"));
}

#[test]
fn init_starter_rejects_non_url_alias() {
    let env = TestEnv::new();
    let proj = project_dir();
    env.bougie()
        .current_dir(proj.path())
        .args(["init", "--starter", "./not-a-url"])
        .assert()
        .failure()
        .stderr(contains("alias"));
}
