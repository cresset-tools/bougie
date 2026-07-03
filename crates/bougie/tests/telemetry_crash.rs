//! Phase-4 crash lane: a panic spools a scrubbed `crash` event and the
//! exit-101 contract holds. Uses the test-fixtures-only panic trigger
//! plus the forced hook (debug builds skip the hook otherwise).
#![cfg(all(unix, feature = "test-fixtures"))]

use assert_cmd::Command;
use tempfile::TempDir;

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

    fn spooled_lines(&self) -> Vec<String> {
        let dir = self.cache.path().join("telemetry").join("spool");
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut lines = Vec::new();
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "ndjson") {
                if let Ok(contents) = std::fs::read_to_string(entry.path()) {
                    lines.extend(contents.lines().map(str::to_owned));
                }
            }
        }
        lines
    }

    fn trigger_panic(&self) -> std::process::Output {
        self.bougie()
            .arg("__telemetry-flush")
            .env("BOUGIE_TEST_PANIC", "1")
            .env("BOUGIE_TELEMETRY_FORCE_CRASH_HOOK", "1")
            .output()
            .unwrap()
    }
}

#[test]
fn panic_spools_scrubbed_crash_event_and_exits_101() {
    let env = Env::new();
    env.bougie().args(["telemetry", "local"]).assert().success();

    let out = env.trigger_panic();
    assert_eq!(out.status.code(), Some(101), "panic exit contract");
    // The default hook still printed the panic to stderr for the user.
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("panicked"),
        "default hook chained"
    );

    let lines = env.spooled_lines();
    let crashes: Vec<serde_json::Value> = lines
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["event"] == "crash")
        .collect();
    assert_eq!(crashes.len(), 1, "one crash event: {lines:?}");
    let crash = &crashes[0];

    assert_eq!(crash["schema"], 1);
    assert_eq!(crash["command"], "__telemetry-flush");
    let fp = crash["fingerprint"].as_str().unwrap();
    assert_eq!(fp.len(), 16);
    assert!(fp.bytes().all(|b| b.is_ascii_hexdigit()));

    let frames = crash["frames"].as_array().unwrap();
    assert!(!frames.is_empty());
    for frame in frames {
        let f = frame.as_str().unwrap();
        assert!(
            !f.contains('/') || f == "[external]",
            "no path-like content in frames: {f}"
        );
    }

    // The scrubber killed the path and the quoted secret; the message
    // shape survives.
    let message = crash["message"].as_str().unwrap_or("");
    assert!(!message.contains("secret"), "{message}");
    assert!(!message.contains("file.php"), "{message}");
    assert!(message.contains("[redacted]"), "{message}");
}

#[test]
fn same_crash_ships_once_per_day() {
    let env = Env::new();
    env.bougie().args(["telemetry", "local"]).assert().success();

    let first = env.trigger_panic();
    let second = env.trigger_panic();
    assert_eq!(first.status.code(), Some(101));
    assert_eq!(second.status.code(), Some(101));

    let crashes = env
        .spooled_lines()
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["event"] == "crash")
        .count();
    assert_eq!(crashes, 1, "fingerprint dedupe: one event for two identical panics");
}

#[test]
fn crash_lane_respects_mode_off() {
    let env = Env::new();
    // Mode never set → off: the hook must record nothing, even forced.
    let out = env.trigger_panic();
    assert_eq!(out.status.code(), Some(101));
    assert!(env.spooled_lines().is_empty(), "off records no crash");
}
