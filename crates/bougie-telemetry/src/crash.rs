//! Crash lane: a chained panic hook that spools a scrubbed `crash`
//! event before the default hook prints to stderr.
//!
//! Installed by `main()` on the normal CLI path only — never on the
//! shim/daemon roles — and in release builds only (the bin decides;
//! tests force it via an explicit flag). The hook must never panic:
//! every step is best-effort, and the previous hook always runs so
//! the user-visible panic output and the exit-101 contract stay
//! untouched.

use crate::clock::UtcHour;
use crate::event::{self, Common, CrashEvent, SCHEMA};
use crate::ids;
use crate::mode::{self, Mode};
use crate::recorder::BinInfo;
use crate::scrub;
use crate::spool::Spool;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

/// Set by the dispatcher right after parse so a crash knows which
/// verb was running (`command_name` returns `&'static str`).
static COMMAND: OnceLock<&'static str> = OnceLock::new();

pub fn set_command(name: &'static str) {
    let _ = COMMAND.set(name);
}

/// Chain the crash recorder in front of the current panic hook.
pub fn install_hook(info: BinInfo) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        record_crash(info, panic_info);
        previous(panic_info);
    }));
}

/// Best-effort crash capture. Consent is resolved *at panic time* so
/// a mode change during the run is honored; everything failing simply
/// means no crash event.
fn record_crash(info: BinInfo, panic_info: &std::panic::PanicHookInfo<'_>) {
    let mode_file = bougie_paths::telemetry_mode_file().ok();
    let consent = mode::resolve_from_env(mode_file.as_deref());
    if consent.mode == Mode::Off {
        return;
    }
    let Ok(paths) = bougie_paths::Paths::from_env() else { return };

    // Frame capture is the heavy part: bounded by MAX_FRAMES before
    // any formatting happens (the panicking thread has a 16 MiB stack,
    // but no reason to gamble).
    let backtrace = backtrace::Backtrace::new();
    let frames = scrub::frames(&backtrace);
    if frames.is_empty() {
        return;
    }
    let fingerprint = scrub::fingerprint(&frames);
    let now = UtcHour::now();
    if !mark_seen(paths.cache(), &fingerprint, &now.date()) {
        return; // this crash already shipped today
    }

    let home = std::env::var("HOME").ok();
    let message = panic_message(panic_info)
        .map(|raw| scrub::message(raw, home.as_deref()))
        .filter(|m| !m.is_empty());

    let config_dir = bougie_paths::config_dir().ok();
    let install_id = config_dir
        .as_deref()
        .and_then(ids::read)
        .unwrap_or_else(|| ids::UNSET.to_owned());
    let event = CrashEvent {
        common: Common {
            schema: SCHEMA,
            event: "crash",
            ts: now.rfc3339(),
            install_id,
            invocation: ids::invocation_id(),
            bougie_version: info.version,
            build_sha: info.build_sha,
            os: event::os(),
            arch: event::arch(),
            libc: event::libc(),
            ci: mode::is_ci(),
            install_method: "unknown",
        },
        command: COMMAND.get().copied().unwrap_or("unknown"),
        fingerprint,
        frames,
        message,
    };
    if let Ok(line) = serde_json::to_string(&event) {
        Spool::new(paths.cache()).append(&now.date(), &line);
    }
}

fn panic_message<'a>(panic_info: &'a std::panic::PanicHookInfo<'_>) -> Option<&'a str> {
    let payload = panic_info.payload();
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
}

/// Per-day fingerprint dedupe via `<cache>/telemetry/crashes-seen`
/// (`<fingerprint> <date>` lines, pruned to the last 50). Returns
/// `true` when this (fingerprint, day) is new and was recorded.
fn mark_seen(cache_root: &Path, fingerprint: &str, date: &str) -> bool {
    let path = cache_root.join("telemetry").join("crashes-seen");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let needle = format!("{fingerprint} {date}");
    if existing.lines().any(|l| l.trim() == needle) {
        return false;
    }
    let mut lines: Vec<&str> = existing.lines().collect();
    lines.push(&needle);
    let keep = lines.len().saturating_sub(50);
    let body = lines[keep..].join("\n");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, body + "\n");
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn seen_marker_dedupes_per_day() {
        let tmp = TempDir::new().unwrap();
        assert!(mark_seen(tmp.path(), "abcd", "2026-07-03"));
        assert!(!mark_seen(tmp.path(), "abcd", "2026-07-03"));
        assert!(mark_seen(tmp.path(), "abcd", "2026-07-04"), "new day, ship again");
        assert!(mark_seen(tmp.path(), "beef", "2026-07-03"), "different crash ships");
    }

    #[test]
    fn seen_file_is_pruned() {
        let tmp = TempDir::new().unwrap();
        for i in 0..80 {
            assert!(mark_seen(tmp.path(), &format!("fp{i:04}"), "2026-07-03"));
        }
        let contents =
            fs::read_to_string(tmp.path().join("telemetry").join("crashes-seen")).unwrap();
        assert!(contents.lines().count() <= 50);
        assert!(contents.contains("fp0079"), "newest entries survive");
    }
}
