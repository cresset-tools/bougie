//! The installer consent snippet (`scripts/install-consent.sh`) is
//! appended to the dist installer at publish time — these tests run it
//! standalone under `sh -u` (the dist script sets `set -u`) and check
//! every non-interactive path. The interactive `/dev/tty` path needs a
//! pty and is covered by manual release verification.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

fn snippet() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install-consent.sh")
}

struct Run {
    home: TempDir,
}

impl Run {
    fn new() -> Self {
        Self { home: TempDir::new().unwrap() }
    }

    fn mode_file(&self) -> PathBuf {
        self.home.path().join("config/bougie/telemetry")
    }

    fn sh(&self) -> Command {
        let mut cmd = Command::new("sh");
        // `-u` mirrors the dist installer's `set -u`; the snippet must
        // survive it.
        cmd.arg("-u").arg(snippet());
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"));
        cmd
    }
}

#[test]
fn snippet_is_posix_sh_clean() {
    let out = Command::new("sh").arg("-n").arg(snippet()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn ci_skips_and_writes_nothing() {
    let run = Run::new();
    let out = run.sh().env("CI", "true").output().unwrap();
    assert!(out.status.success());
    assert!(!run.mode_file().exists());
}

#[test]
fn explicit_env_skips_and_writes_nothing() {
    let run = Run::new();
    let out = run.sh().env("CI", "true").env("BOUGIE_TELEMETRY", "on").output().unwrap();
    assert!(out.status.success());
    assert!(!run.mode_file().exists());
}

#[test]
fn do_not_track_records_a_decline() {
    let run = Run::new();
    let out = run.sh().env("DO_NOT_TRACK", "1").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let contents = std::fs::read_to_string(run.mode_file()).unwrap();
    let mut parts = contents.split_ascii_whitespace();
    assert_eq!(parts.next(), Some("off"));
    let date = parts.next().unwrap();
    assert_eq!(date.len(), 10, "yyyy-mm-dd: {date}");
    assert_eq!(parts.next(), Some("1"), "consent version");
}

#[test]
fn existing_mode_file_is_never_touched() {
    let run = Run::new();
    std::fs::create_dir_all(run.mode_file().parent().unwrap()).unwrap();
    std::fs::write(run.mode_file(), "on 2026-01-01 1\n").unwrap();
    // Even a DNT run must not overwrite a recorded consent.
    let out = run.sh().env("DO_NOT_TRACK", "1").output().unwrap();
    assert!(out.status.success());
    assert_eq!(std::fs::read_to_string(run.mode_file()).unwrap(), "on 2026-01-01 1\n");
}

#[cfg(target_os = "linux")]
#[test]
fn no_controlling_tty_leaves_mode_unset() {
    let run = Run::new();
    // `setsid` detaches from the controlling terminal, so the
    // `/dev/tty` probe fails and the snippet leaves the mode unset for
    // bougie's own first-run prompt.
    let out = Command::new("setsid")
        .arg("sh")
        .arg("-u")
        .arg(snippet())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", run.home.path())
        .env("XDG_CONFIG_HOME", run.home.path().join("config"))
        .output();
    let Ok(out) = out else {
        // setsid unavailable: nothing to assert.
        return;
    };
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!run.mode_file().exists());
}

#[test]
fn appended_after_failing_entrypoint_never_runs() {
    // Simulates the real layout: dist entrypoint line, then the
    // snippet. A failed install (`|| exit 1`) must skip consent.
    let run = Run::new();
    let combined = run.home.path().join("installer.sh");
    let mut script = String::from("#!/bin/sh\nset -u\nmain() { return 1; }\nmain \"$@\" || exit 1\n");
    script.push_str(&std::fs::read_to_string(snippet()).unwrap());
    std::fs::write(&combined, script).unwrap();
    let out = Command::new("sh")
        .arg(&combined)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", run.home.path())
        .env("XDG_CONFIG_HOME", run.home.path().join("config"))
        .env("DO_NOT_TRACK", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "install failure propagates");
    assert!(!run.mode_file().exists(), "consent never runs after a failed install");
}
