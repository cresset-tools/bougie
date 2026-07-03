//! Parent-side flush trigger: decide whether a flush is due and spawn
//! the detached `__telemetry-flush` child.
//!
//! The parent never waits and never uploads. The child is spawned with
//! null stdio and detaches itself (`setsid` + nice 19 in
//! [`crate::flush::deprioritize`]); on Windows the detachment and the
//! below-normal priority both ride the spawn's creation flags.

use crate::spool::Spool;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Spool size that makes a flush due regardless of age.
pub const FLUSH_BYTES: u64 = 64 * 1024;

/// Minimum seconds between spawn attempts, so a dead collector doesn't
/// buy a child process per command.
pub const ATTEMPT_COOLDOWN_SECS: i64 = 15 * 60;

/// A flush is due when the spool is heavy, or anything in it is from a
/// previous UTC day (the ">24h" rule at spool-date granularity).
pub fn flush_due(spool: &Spool, today: &str) -> bool {
    if spool.total_bytes() > FLUSH_BYTES {
        return true;
    }
    spool.oldest_date().is_some_and(|d| d.as_str() < today)
}

fn attempt_marker(cache_root: &Path) -> std::path::PathBuf {
    cache_root.join("telemetry").join("last-flush-attempt")
}

/// Record that a flush attempt started (written by the child under the
/// flush lock). Best-effort.
pub fn write_attempt_marker(cache_root: &Path) {
    let path = attempt_marker(cache_root);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, now_secs().to_string());
}

fn attempt_recently(cache_root: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(attempt_marker(cache_root)) else {
        return false;
    };
    raw.trim()
        .parse::<i64>()
        .is_ok_and(|then| now_secs() - then < ATTEMPT_COOLDOWN_SECS)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// Spawn the detached flush child if one is due and none was attempted
/// recently. Best-effort: every failure is swallowed.
pub fn maybe_spawn_flush(cache_root: &Path, today: &str) {
    let spool = Spool::new(cache_root);
    if !flush_due(&spool, today) || attempt_recently(cache_root) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__telemetry-flush")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP |
        // BELOW_NORMAL_PRIORITY_CLASS — no console, detached from
        // Ctrl-C, and deprioritized at spawn (no child cooperation
        // needed on Windows).
        cmd.creation_flags(0x0800_0000 | 0x0000_0200 | 0x0000_4000);
    }
    match cmd.spawn() {
        // Not waited on: the child outlives this invocation by design
        // (Unix: reparented to init once we exit; it setsids itself).
        Ok(child) => drop(child),
        Err(err) => tracing::debug!("telemetry flush spawn failed: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn flush_due_on_size() {
        let tmp = TempDir::new().unwrap();
        let spool = Spool::new(tmp.path());
        assert!(!flush_due(&spool, "2026-07-03"), "empty spool: not due");
        spool.append("2026-07-03", &"x".repeat(65 * 1024));
        assert!(flush_due(&spool, "2026-07-03"));
    }

    #[test]
    fn flush_due_on_age() {
        let tmp = TempDir::new().unwrap();
        let spool = Spool::new(tmp.path());
        spool.append("2026-07-02", "{}");
        assert!(flush_due(&spool, "2026-07-03"), "yesterday's events are due");
        let tmp2 = TempDir::new().unwrap();
        let spool2 = Spool::new(tmp2.path());
        spool2.append("2026-07-03", "{}");
        assert!(!flush_due(&spool2, "2026-07-03"), "today's small spool can wait");
    }

    #[test]
    fn attempt_cooldown() {
        let tmp = TempDir::new().unwrap();
        assert!(!attempt_recently(tmp.path()));
        write_attempt_marker(tmp.path());
        assert!(attempt_recently(tmp.path()));
    }
}
