//! Phase-2a flush integration: gzip NDJSON batches reach the collector
//! endpoint, spool files are deleted on 2xx and retained on failure.
#![cfg(unix)]

use assert_cmd::Command;
use std::io::Read as _;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
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
            .env_remove("BOUGIE_TELEMETRY")
            .env_remove("DO_NOT_TRACK")
            .env_remove("RUST_LOG");
        cmd
    }

    fn spool_dir(&self) -> std::path::PathBuf {
        self.cache.path().join("telemetry").join("spool")
    }

    fn spool_files(&self) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(self.spool_dir())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "ndjson"))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn seed_spool(&self, date: &str, lines: &[&str]) {
        std::fs::create_dir_all(self.spool_dir()).unwrap();
        std::fs::write(
            self.spool_dir().join(format!("{date}.ndjson")),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn flush_uploads_gzip_ndjson_and_empties_spool() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/batch"))
        .and(header("content-encoding", "gzip"))
        .and(header("content-type", "application/x-ndjson"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let env = Env::new();
    env.bougie().args(["telemetry", "on"]).assert().success();
    env.seed_spool("2026-07-01", &[r#"{"schema":1,"n":1}"#, r#"{"schema":1,"n":2}"#]);

    env.bougie()
        .arg("__telemetry-flush")
        .env("BOUGIE_TELEMETRY_URL", format!("{}/v1/batch", server.uri()))
        .assert()
        .success();

    assert!(env.spool_files().is_empty(), "2xx deletes flushed files");

    // Decode the received body: gzip'd NDJSON, lines preserved.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let mut decoder = flate2::read::GzDecoder::new(&requests[0].body[..]);
    let mut body = String::new();
    decoder.read_to_string(&mut body).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains(r#""n":1"#));
    // User agent carries only the bougie version.
    let ua = requests[0].headers.get("user-agent").unwrap().to_str().unwrap();
    assert!(ua.starts_with("bougie/"), "{ua}");
}

#[tokio::test(flavor = "multi_thread")]
async fn flush_failure_retains_spool() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/batch"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let env = Env::new();
    env.bougie().args(["telemetry", "on"]).assert().success();
    env.seed_spool("2026-07-01", &[r#"{"schema":1,"n":1}"#]);

    // The child reports the failure via its exit status…
    env.bougie()
        .arg("__telemetry-flush")
        .env("BOUGIE_TELEMETRY_URL", format!("{}/v1/batch", server.uri()))
        .assert()
        .failure();

    // …but the events survive for the next attempt.
    assert_eq!(env.spool_files().len(), 1, "failed upload retains the file");
}

#[tokio::test(flavor = "multi_thread")]
async fn flush_is_a_noop_below_on() {
    let server = MockServer::start().await;
    // No mock mounted: any request would 404 and fail the run.
    let env = Env::new();
    env.bougie().args(["telemetry", "local"]).assert().success();
    env.seed_spool("2026-07-01", &[r#"{"schema":1,"n":1}"#]);

    env.bougie()
        .arg("__telemetry-flush")
        .env("BOUGIE_TELEMETRY_URL", format!("{}/v1/batch", server.uri()))
        .assert()
        .success();

    assert_eq!(env.spool_files().len(), 1, "local mode never uploads");
    assert!(server.received_requests().await.unwrap().is_empty());
}
