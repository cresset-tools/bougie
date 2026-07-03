//! Child-side flush: drain the spool to the collector.
//!
//! Runs only inside the detached `bougie __telemetry-flush` subprocess
//! (spawned by [`crate::spawn`]), never in-band with a user command.
//! First thing it does is drop its own scheduling priority — the flush
//! is pure housekeeping and must never compete with the user's work.

use crate::mode::{self, Mode};
use crate::spool::Spool;
use eyre::{Result, WrapErr};
use std::fs;
use std::io::Write as _;
use std::time::Duration;

/// Default collector endpoint. Overridable for tests/mirrors via
/// `BOUGIE_TELEMETRY_URL`.
pub const DEFAULT_ENDPOINT: &str = "https://telemetry.bougie.tools/v1/batch";

/// Max raw (pre-gzip) bytes per upload request.
pub const MAX_BATCH_BYTES: usize = 256 * 1024;

/// Upload timeout. Deliberately short: the spool persists, the next
/// flush retries, and at nice 19 on a busy machine we'd rather give up
/// than linger.
pub const TIMEOUT: Duration = Duration::from_secs(5);

pub fn endpoint() -> String {
    std::env::var("BOUGIE_TELEMETRY_URL").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_owned())
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FlushStats {
    pub files: usize,
    pub events: usize,
    pub bytes: u64,
}

/// Deprioritize the current process as far as safe Rust reaches:
/// CPU nice 19, plus the Linux autogroup nice (a `setsid` child is its
/// own session ⇒ its own autogroup, where per-process nice alone
/// doesn't weigh against other groups — `/proc/self/autogroup` accepts
/// a nice value via plain file I/O). Best-effort throughout.
pub fn deprioritize() {
    #[cfg(unix)]
    {
        // Detach from the parent's session first (daemon.rs precedent;
        // EPERM when already a leader is benign).
        let _ = rustix::process::setsid();
        let _ = rustix::process::nice(19);
    }
    #[cfg(target_os = "linux")]
    {
        let _ = fs::write("/proc/self/autogroup", "19");
    }
}

/// Drain the spool: gzip NDJSON batches, POST, delete on 2xx. Holds
/// the flush lock so concurrent flushers no-op; re-checks the consent
/// mode so a mode flipped between spawn and execution is honored.
pub fn run_flush(paths: &bougie_paths::Paths, version: &str) -> Result<FlushStats> {
    let mode_file = bougie_paths::telemetry_mode_file().ok();
    let consent = mode::resolve_from_env(mode_file.as_deref());
    if consent.mode != Mode::On {
        return Ok(FlushStats::default());
    }
    let spool = Spool::new(paths.cache());
    let lock_path = paths.cache().join("telemetry").join("flush.lock");
    // Another live flusher (lock held) means ours has nothing to do.
    let Ok(_guard) = bougie_fs::lock::ExclusiveGuard::acquire(&lock_path, Duration::ZERO)
    else {
        return Ok(FlushStats::default());
    };
    crate::spawn::write_attempt_marker(paths.cache());

    let client = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(format!("bougie/{version}"))
        .build()
        .wrap_err("building telemetry http client")?;
    let url = endpoint();

    let mut stats = FlushStats::default();
    for file in spool.files() {
        let Ok(contents) = fs::read_to_string(&file) else { continue };
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        if lines.is_empty() {
            let _ = fs::remove_file(&file);
            continue;
        }
        for batch in batches(&lines, MAX_BATCH_BYTES) {
            let body = gzip(batch.as_bytes())?;
            let response = client
                .post(&url)
                .header(reqwest::header::CONTENT_TYPE, "application/x-ndjson")
                .header(reqwest::header::CONTENT_ENCODING, "gzip")
                .body(body)
                .send()
                .wrap_err("uploading telemetry batch")?;
            if !response.status().is_success() {
                // Leave this file (and everything after it) for the
                // next flush; the spool caps bound the damage.
                return Err(eyre::eyre!(
                    "collector answered {} for {url}",
                    response.status()
                ));
            }
            stats.bytes += batch.len() as u64;
        }
        stats.files += 1;
        stats.events += lines.len();
        let _ = fs::remove_file(&file);
    }
    Ok(stats)
}

/// Group lines into newline-joined batches of at most `max` raw bytes
/// (a single oversized line still ships alone rather than being lost).
fn batches(lines: &[&str], max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in lines {
        if !current.is_empty() && current.len() + line.len() + 1 > max {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).wrap_err("gzipping telemetry batch")?;
    encoder.finish().wrap_err("finishing telemetry gzip stream")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_respect_size_cap() {
        let lines = ["a".repeat(100), "b".repeat(100), "c".repeat(100)];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let batched = batches(&refs, 150);
        assert_eq!(batched.len(), 3);
        let batched = batches(&refs, 250);
        assert_eq!(batched.len(), 2);
        assert_eq!(batched[0].len(), 201); // two lines + newline
    }

    #[test]
    fn oversized_single_line_still_ships() {
        let big = "x".repeat(1000);
        let batched = batches(&[big.as_str()], 100);
        assert_eq!(batched.len(), 1);
    }

    #[test]
    fn gzip_round_trips() {
        use std::io::Read as _;
        let body = gzip(b"{\"a\":1}\n{\"b\":2}").unwrap();
        let mut decoder = flate2::read::GzDecoder::new(&body[..]);
        let mut out = String::new();
        decoder.read_to_string(&mut out).unwrap();
        assert_eq!(out, "{\"a\":1}\n{\"b\":2}");
    }
}
