//! `bougie diagnose`: last-failure capture + hint line, report
//! assembly (services / daemon / ports sections), scrubbing, the
//! $EDITOR review pass, explicit-consent upload of the schema-2
//! markdown envelope, the --issue lane, and independence from the
//! telemetry mode.
#![cfg(unix)]

use assert_cmd::Command;
use std::path::{Path, PathBuf};
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
            .env_remove("VISUAL")
            .env_remove("EDITOR")
            .env_remove("COMPOSER_AUTH")
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

    /// A project dir declaring rabbitmq, with a recorded failure from
    /// inside it (so `project_dir` lands in last-failure.json).
    fn project_with_failure(&self) -> PathBuf {
        let proj = self.home.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("bougie.toml"), "[services]\nrabbitmq = \"*\"\n").unwrap();
        let out = self.bougie().arg("tree").current_dir(&proj).output().unwrap();
        assert!(!out.status.success(), "tree without composer.json should fail");
        proj
    }

    /// Plant a service log under the daemon's on-disk layout.
    fn plant_service_log(&self, service: &str, contents: &str) {
        let log_dir = self.home.path().join("state/services").join(service).join("log");
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(log_dir.join(format!("{service}.log")), contents).unwrap();
    }

    /// A fake $EDITOR: a shell script receiving the draft path as $1.
    fn editor_script(&self, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = self.home.path().join("fake-editor.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn assert_no_daemon_spawned(&self, stderr: &str) {
        assert!(
            !self.home.path().join("state/bougied.sock").exists(),
            "diagnose must never spawn bougied"
        );
        assert!(!stderr.contains("starting bougied"), "{stderr}");
    }
}

fn read_issue_file(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("bougie-diagnose.md")).expect("bougie-diagnose.md written")
}

#[test]
fn nothing_recorded_yet_is_a_clear_failure() {
    let env = Env::new();
    let out = env.bougie().arg("diagnose").arg("--issue").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing to report"));
}

#[test]
fn issue_lane_writes_report_file_without_any_network() {
    let env = Env::new();
    env.record_failure();

    let dir = env.home.path().join("empty");
    let out = env.bougie().args(["diagnose", "--issue"]).current_dir(&dir).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // Non-tty → no editor; the stdout print is the review …
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("# bougie diagnostic report"));
    assert!(stdout.contains("## last failure"));
    // Error detail is the point here — the chain survives verbatim.
    assert!(stdout.contains("category:"));
    // … and the file is what gets attached to the issue.
    let report = read_issue_file(&dir);
    assert!(report.contains("## last failure"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("bougie-diagnose.md"), "{stderr}");
    assert!(stderr.contains("issues/new"), "{stderr}");
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
async fn yes_uploads_markdown_envelope_and_prints_id() {
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
    // Schema-2 envelope: fixed machine facts + the markdown report.
    assert_eq!(body["schema_version"], 2);
    assert!(body["os"].is_string());
    let report = body["report_md"].as_str().expect("report_md is the payload");
    assert!(report.contains("## last failure"));
    assert!(report.contains("category:"), "full error chain ships");
    // Home directory folded to ~ as the courtesy pass — nowhere in
    // the whole envelope.
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
    let dir = env.home.path().join("rerun");
    std::fs::create_dir_all(&dir).unwrap();
    let out = env
        .bougie()
        .args(["diagnose", "--issue", "--", "cache", "dir"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report = read_issue_file(&dir);
    assert!(report.contains("## re-run with debug logging"), "{report}");
    assert!(report.contains("exit:    0"), "{report}");
}

// ---------- report v2: services / daemon / ports sections ----------

#[test]
fn report_carries_service_log_tail_and_ports_offline() {
    let env = Env::new();
    let proj = env.project_with_failure();
    env.plant_service_log(
        "rabbitmq",
        "starting broker\nBOOT FAILED: Address already in use — 127.0.0.1:5672\n",
    );

    let out = env.bougie().args(["diagnose", "--issue"]).current_dir(&proj).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report = read_issue_file(&proj);

    // Project + declared services in the environment section.
    assert!(report.contains("declared services: rabbitmq *"), "{report}");
    // The service section shipped the log tail — the motivating case.
    assert!(report.contains("### rabbitmq (declared: *)"), "{report}");
    assert!(report.contains("BOOT FAILED: Address already in use"), "{report}");
    // No daemon → state is honest about it.
    assert!(report.contains("state unknown (daemon not running)"), "{report}");
    assert!(report.contains("bougied: not running"), "{report}");
    // Ports table probes the catalog binding + the epmd sidecar.
    assert!(report.contains("| 5672 | rabbitmq |"), "{report}");
    assert!(report.contains("| 4369 | rabbitmq (epmd) |"), "{report}");

    env.assert_no_daemon_spawned(&String::from_utf8_lossy(&out.stderr));
}

#[test]
fn last_failure_records_project_dir() {
    let env = Env::new();
    let proj = env.project_with_failure();
    let raw =
        std::fs::read_to_string(env.cache.path().join("telemetry/last-failure.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["schema"], 2);
    assert_eq!(v["project_dir"], proj.to_string_lossy().as_ref());
}

#[test]
fn diagnose_finds_project_from_last_failure_when_run_elsewhere() {
    let env = Env::new();
    let _proj = env.project_with_failure();
    env.plant_service_log("rabbitmq", "distinctive-rabbitmq-line\n");

    // Run diagnose from a completely unrelated directory.
    let elsewhere = env.home.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let out =
        env.bougie().args(["diagnose", "--issue"]).current_dir(&elsewhere).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report = read_issue_file(&elsewhere);
    assert!(report.contains("distinctive-rabbitmq-line"), "{report}");
}

#[test]
fn tenant_secrets_are_scrubbed_from_log_tails() {
    let env = Env::new();
    let proj = env.project_with_failure();
    let ledger_dir = env.home.path().join("state/services/rabbitmq");
    std::fs::create_dir_all(&ledger_dir).unwrap();
    std::fs::write(
        ledger_dir.join("tenants.json"),
        r#"{"schema_version":1,"tenant":"proj","project":"/p","created_at":"t","secrets":{"password":"sup3rs3cretpw"}}"#,
    )
    .unwrap();
    env.plant_service_log("rabbitmq", "auth attempt for proj with sup3rs3cretpw failed\n");

    let out = env.bougie().args(["diagnose", "--issue"]).current_dir(&proj).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report = read_issue_file(&proj);
    assert!(!report.contains("sup3rs3cretpw"), "secret must not ship: {report}");
    assert!(report.contains("«redacted:tenant-secret»"), "{report}");
}

// ---------- the $EDITOR pass ----------

#[tokio::test(flavor = "multi_thread")]
async fn editor_note_and_redaction_are_authoritative() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/diagnose"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "diag-edited"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let env = Env::new();
    let proj = env.project_with_failure();
    env.plant_service_log("rabbitmq", "keep-this-line\nPRIVATE-THING do-not-ship\n");
    // Append a note AND redact the private line.
    let editor = env.editor_script(
        r#"grep -v PRIVATE-THING "$1" > "$1.tmp" && mv "$1.tmp" "$1"
echo "note-from-the-editor" >> "$1""#,
    );

    let out = env
        .bougie()
        .args(["diagnose", "--yes", "--edit"])
        .current_dir(&proj)
        .env("EDITOR", &editor)
        .env("BOUGIE_DIAGNOSE_URL", format!("{}/v1/diagnose", server.uri()))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let report = body["report_md"].as_str().unwrap();
    assert!(report.contains("keep-this-line"), "{report}");
    assert!(report.contains("note-from-the-editor"), "{report}");
    assert!(!report.contains("PRIVATE-THING"), "in-editor redaction is authoritative");
    // The instruction header never ships.
    assert!(!report.contains("review before sending"), "{report}");
    assert!(!report.contains(">8"), "{report}");
    // Draft is gone after a successful send.
    let leftover: Vec<_> = std::fs::read_dir(env.cache.path().join("telemetry"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("diagnose-draft-"))
        .collect();
    assert!(leftover.is_empty(), "draft cleaned up: {leftover:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn saving_an_empty_report_aborts_without_a_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/diagnose"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let env = Env::new();
    env.record_failure();
    let editor = env.editor_script(r#": > "$1""#);

    let out = env
        .bougie()
        .args(["diagnose", "--yes", "--edit"])
        .env("EDITOR", &editor)
        .env("BOUGIE_DIAGNOSE_URL", format!("{}/v1/diagnose", server.uri()))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("empty report"));
}

#[test]
fn failing_editor_keeps_the_draft() {
    let env = Env::new();
    env.record_failure();
    let editor = env.editor_script("exit 7");

    let out = env
        .bougie()
        .args(["diagnose", "--yes", "--edit", "--issue"])
        .env("EDITOR", &editor)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("draft kept at"), "{stderr}");
    let drafts: Vec<_> = std::fs::read_dir(env.cache.path().join("telemetry"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("diagnose-draft-"))
        .collect();
    assert_eq!(drafts.len(), 1, "draft survives an editor failure");
}
