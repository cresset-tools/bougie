//! Phase-1 telemetry integration: mode precedence, spooled command
//! events, CLI surface, and json-v1 stdout purity.
//!
//! Unix-only: the mode file lives under `XDG_CONFIG_HOME`, which is the
//! Unix resolution path; Windows CI runs only `windows_smoke`.
#![cfg(unix)]

use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;

/// Isolated env: `BOUGIE_HOME` / `BOUGIE_CACHE` plus `XDG_CONFIG_HOME`
/// (the telemetry mode file's root). Deliberately not `common::TestEnv`
/// — that module drags in Unix service fixtures this test doesn't need.
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
            .env_remove("CI")
            .env_remove("RUST_LOG");
        cmd
    }

    fn mode_file(&self) -> std::path::PathBuf {
        self.config.path().join("bougie").join("telemetry")
    }

    fn spool_dir(&self) -> std::path::PathBuf {
        self.cache.path().join("telemetry").join("spool")
    }

    fn spooled_lines(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.spool_dir()) else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        for entry in entries.flatten() {
            if let Ok(contents) = std::fs::read_to_string(entry.path()) {
                lines.extend(contents.lines().map(str::to_owned));
            }
        }
        lines
    }
}

fn json_stdout(cmd: &mut Command) -> serde_json::Value {
    let out = cmd.arg("--format").arg("json-v1").output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("stdout is pure JSON")
}

#[test]
fn default_mode_is_off_and_nothing_spools() {
    let env = Env::new();
    let status = json_stdout(env.bougie().arg("telemetry"));
    assert_eq!(status["mode"], "off");
    assert_eq!(status["source"], "unset");

    env.bougie().args(["cache", "dir"]).assert().success();
    assert!(env.spooled_lines().is_empty(), "off must record nothing");
}

#[test]
fn local_mode_spools_command_events_with_clean_json_stdout() {
    let env = Env::new();
    env.bougie().args(["telemetry", "local"]).assert().success();
    assert!(env.mode_file().is_file());

    // A cheap offline command must both emit pure JSON on stdout and
    // spool exactly one command event.
    let out = env
        .bougie()
        .args(["cache", "dir", "--format", "json-v1"])
        .output()
        .unwrap();
    assert!(out.status.success());
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .expect("json-v1 stdout stays parseable with telemetry enabled");

    let lines = env.spooled_lines();
    assert_eq!(lines.len(), 1, "one invocation, one event: {lines:?}");
    let event: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(event["schema"], 1);
    assert_eq!(event["event"], "command");
    assert_eq!(event["name"], "cache");
    assert_eq!(event["outcome"], "ok");
    assert_eq!(event["exit_code"], 0);
    // `local` mints no persistent id.
    assert_eq!(event["install_id"], "unset");
    assert!(
        event["ts"].as_str().unwrap().ends_with(":00:00Z"),
        "timestamps are hour-truncated: {}",
        event["ts"]
    );
}

#[test]
fn env_var_overrides_file_and_do_not_track_beats_env() {
    let env = Env::new();
    env.bougie().args(["telemetry", "on"]).assert().success();

    let status = json_stdout(env.bougie().arg("telemetry").env("BOUGIE_TELEMETRY", "off"));
    assert_eq!(status["mode"], "off");
    assert_eq!(status["source"], "BOUGIE_TELEMETRY");

    let status = json_stdout(
        env.bougie().arg("telemetry").env("BOUGIE_TELEMETRY", "on").env("DO_NOT_TRACK", "1"),
    );
    assert_eq!(status["mode"], "off");
    assert_eq!(status["source"], "DO_NOT_TRACK");

    // Truthy alias for the env var.
    std::fs::remove_file(env.mode_file()).unwrap();
    let status = json_stdout(env.bougie().arg("telemetry").env("BOUGIE_TELEMETRY", "1"));
    assert_eq!(status["mode"], "on");
}

#[test]
fn do_not_track_stops_recording_even_in_local_mode() {
    let env = Env::new();
    env.bougie().args(["telemetry", "local"]).assert().success();
    env.bougie().args(["cache", "dir"]).env("DO_NOT_TRACK", "1").assert().success();
    assert!(env.spooled_lines().is_empty());
}

#[test]
fn on_mints_install_id_and_reset_rotates_it() {
    let env = Env::new();
    let set = json_stdout(env.bougie().args(["telemetry", "on"]));
    let first = set["install_id"].as_str().expect("on mints an id").to_owned();
    assert_eq!(first.len(), 36);

    // Events recorded under `on` carry the id.
    env.bougie().args(["cache", "dir"]).assert().success();
    let lines = env.spooled_lines();
    let event: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(event["install_id"], first.as_str());

    let reset = json_stdout(env.bougie().args(["telemetry", "reset"]));
    let rotated = reset["install_id"].as_str().expect("reset under `on` re-mints");
    assert_ne!(rotated, first);
    assert!(env.spooled_lines().is_empty(), "reset purges the spool");
}

#[test]
fn telemetry_log_prints_spooled_events() {
    let env = Env::new();
    env.bougie().args(["telemetry", "local"]).assert().success();
    env.bougie().args(["cache", "dir"]).assert().success();

    let log = json_stdout(env.bougie().args(["telemetry", "log"]));
    let events = log["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["name"], "cache");
}

#[test]
fn failed_command_records_outcome_category() {
    let env = Env::new();
    env.bougie().args(["telemetry", "local"]).assert().success();
    // `bougie tree` outside a project fails deterministically and
    // offline. (`run` is NOT a safe trigger here: outside a project it
    // falls back to an ephemeral system PHP, so it *succeeds* on CI
    // runners with php preinstalled.)
    let out = env
        .bougie()
        .arg("tree")
        .current_dir(empty_dir(&env))
        .output()
        .unwrap();
    assert!(!out.status.success());

    let lines = env.spooled_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let event: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(event["name"], "tree");
    assert_ne!(event["outcome"], "ok");
    assert_ne!(event["exit_code"], 0);
}

fn empty_dir(env: &Env) -> &Path {
    // Reuse the isolated home tempdir's parent-safe space: an empty
    // subdir with no composer.json.
    let dir = env.home.path().join("empty-project");
    std::fs::create_dir_all(&dir).unwrap();
    // Leak-free: lives inside the TempDir, cleaned on drop.
    Box::leak(dir.into_boxed_path())
}

#[test]
fn parse_failure_spools_a_usage_event_named_after_the_verb() {
    let env = Env::new();
    env.bougie().args(["telemetry", "local"]).assert().success();

    let out = env.bougie().args(["sync", "--definitely-not-a-flag"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));

    let lines = env.spooled_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let event: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(event["event"], "command");
    assert_eq!(event["name"], "sync");
    assert_eq!(event["outcome"], "usage");
    assert_eq!(event["exit_code"], 2);
}

#[test]
fn typoed_verb_records_unknown_and_help_records_nothing() {
    let env = Env::new();
    env.bougie().args(["telemetry", "local"]).assert().success();

    // The typo'd token itself must never reach the spool — the event
    // name degrades to the vocabulary's `unknown`.
    let out = env.bougie().arg("sylc").output().unwrap();
    assert_eq!(out.status.code(), Some(2));

    // Help and version — flag or bare-invocation form — are clap
    // succeeding, not user mistakes: no events.
    env.bougie().arg("--help").assert().success();
    env.bougie().arg("--version").assert().success();
    let _ = env.bougie().output().unwrap();

    let lines = env.spooled_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let event: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(event["name"], "unknown");
    assert_eq!(event["outcome"], "usage");
    assert!(!lines[0].contains("sylc"), "typo must not reach the wire: {}", lines[0]);
}
