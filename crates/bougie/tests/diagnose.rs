//! Phase-5 `bougie diagnose`: last-failure capture + hint line, report
//! assembly and review, explicit-consent upload, --issue lane, and
//! independence from the telemetry mode.
#![cfg(unix)]

use assert_cmd::Command;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Env {
    home: TempDir,
    cache: TempDir,
    config: TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            home: TempDir::new().unwrap(),
            cache: TempDir::new().unwrap(),
            config: TempDir::new().unwrap(),
        }
    }

    fn bougie(&self) -> Command {
        let mut cmd = Command::cargo_bin("bougie").expect("bougie binary");
        cmd.env("BOUGIE_HOME", self.home.path())
            .env("BOUGIE_CACHE", self.cache.path())
            .env("XDG_CONFIG_HOME", self.config.path())
            // The failure below happens inside this dir, so the
            // courtesy home-folding pass has something to fold.
            .env("HOME", self.home.path())
            .env_remove("BOUGIE_TELEMETRY")
            .env_remove("DO_NOT_TRACK")
            .env_remove("RUST_LOG");
        cmd
    }

    /// Produce a recorded failure: `bougie tree` outside a project
    /// fails fast and offline.
    fn record_failure(&self) {
        let dir = self.home.path().join("empty");
        std::fs::create_dir_all(&dir).unwrap();
        let out = self.bougie().arg("tree").current_dir(&dir).output().unwrap();
        assert!(!out.status.success(), "tree in an empty dir should fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("bougie diagnose"),
            "failure output points at diagnose: {stderr}"
        );
        assert!(
            self.cache.path().join("telemetry/last-failure.json").is_file(),
            "last failure recorded"
        );
    }
}

#[test]
fn nothing_recorded_yet_is_a_clear_failure() {
    let env = Env::new();
    let out = env.bougie().arg("diagnose").arg("--issue").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing to report"));
}

#[test]
fn issue_lane_prints_report_without_any_network() {
    let env = Env::new();
    env.record_failure();

    let out = env.bougie().args(["diagnose", "--issue"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("# bougie diagnostic report"));
    assert!(stdout.contains("## last failure"));
    // Error detail is the point here — the chain survives verbatim.
    assert!(stdout.contains("category:"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("issues/new"));
}

#[test]
fn non_interactive_upload_requires_explicit_yes() {
    let env = Env::new();
    env.record_failure();
    // No tty, no --yes, no --issue → refuse to send anything.
    let out = env.bougie().arg("diagnose").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no terminal"));
}

#[tokio::test(flavor = "multi_thread")]
async fn yes_uploads_json_report_and_prints_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/diagnose"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "diag-a1b2c3"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let env = Env::new();
    env.record_failure();

    let out = env
        .bougie()
        .args(["diagnose", "--yes"])
        .env("BOUGIE_DIAGNOSE_URL", format!("{}/v1/diagnose", server.uri()))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("diag-a1b2c3"));

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["schema_version"], 1);
    let chain = body["failure"]["chain"].as_array().unwrap();
    assert!(!chain.is_empty(), "full error chain ships");
    // Home directory folded to ~ as the courtesy pass.
    let home = env.home.path().to_string_lossy().into_owned();
    assert!(
        !serde_json::to_string(&body).unwrap().contains(&home),
        "home dir folded to ~"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnose_works_with_telemetry_hard_off() {
    // DO_NOT_TRACK kills telemetry entirely; diagnose is
    // correspondence and still works.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/diagnose"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "diag-dnt"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let env = Env::new();
    env.record_failure();
    let out = env
        .bougie()
        .args(["diagnose", "--yes"])
        .env("DO_NOT_TRACK", "1")
        .env("BOUGIE_DIAGNOSE_URL", format!("{}/v1/diagnose", server.uri()))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn rerun_lane_captures_debug_stderr() {
    let env = Env::new();
    let out = env
        .bougie()
        .args(["diagnose", "--issue", "--", "cache", "dir"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("## re-run with debug logging"));
    assert!(stdout.contains("exit:    0"));
}
