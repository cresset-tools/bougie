//! In-process enrichment probe: perf + ecosystem fields commands
//! attach to their own invocation's `command` event.
//!
//! Commands push what they already have in hand (`sync` knows the
//! resolved PHP, the dep counts, the phase timings); the recorder
//! drains the probe when it writes the event. Everything here obeys
//! the allowlist discipline: names pass a closed vocabulary, versions
//! are truncated to minor, counts are bucketed, and the ecosystem set
//! is throttled to once per project per week — deduped against a
//! marker under the project's state dir, so the project hash itself
//! never leaves the machine.

use crate::event::Enrichment;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Ecosystem fields ship at most once per project per this interval.
pub const ECOSYSTEM_INTERVAL: Duration = Duration::from_hours(7 * 24);

/// Dev-service names bougie provisions — the full catalog.
pub const SERVICE_VOCAB: &[&str] =
    &["mariadb", "redis", "opensearch", "rabbitmq", "mailpit", "mkcert", "server"];

/// PHP extension names that may appear on events. Anything a project
/// uses outside this list (a private or locally-built extension) is
/// dropped, not sent. Additions happen by PR against TELEMETRY.md.
pub const EXTENSION_VOCAB: &[&str] = &[
    "amqp", "apcu", "ast", "bcmath", "bz2", "calendar", "curl", "dba", "dom", "ds",
    "enchant", "event", "exif", "ffi", "fileinfo", "ftp", "gd", "gettext", "gmp",
    "gnupg", "iconv", "igbinary", "imagick", "imap", "intl", "ldap", "mbstring",
    "memcached", "mongodb", "msgpack", "mysqli", "oci8", "opcache", "openswoole",
    "pcntl", "pdo_mysql", "pdo_pgsql", "pdo_sqlite", "pdo_sqlsrv", "pgsql", "phar",
    "posix", "protobuf", "pspell", "redis", "shmop", "simplexml", "snmp", "soap",
    "sockets", "sodium", "sqlite3", "sqlsrv", "ssh2", "swoole", "sysvmsg", "sysvsem",
    "sysvshm", "tidy", "uuid", "xdebug", "xhprof", "xml", "xmlreader", "xmlwriter",
    "xsl", "yaml", "zip", "zstd",
];

#[derive(Debug, Default)]
pub struct ProbeData {
    pub enrich: Enrichment,
    /// Throttle marker for the ecosystem fields (typically
    /// `<state>/projects/<hash>/telemetry-last-snapshot`). `None`
    /// means no throttle applies.
    pub ecosystem_marker: Option<PathBuf>,
}

fn cell() -> &'static Mutex<ProbeData> {
    static CELL: OnceLock<Mutex<ProbeData>> = OnceLock::new();
    CELL.get_or_init(Mutex::default)
}

/// Push enrichment onto the current invocation. Callers only ever add
/// fields they hold; the recorder drains everything at event time.
pub fn record(f: impl FnOnce(&mut ProbeData)) {
    if let Ok(mut guard) = cell().lock() {
        f(&mut guard);
    }
}

/// Drain the probe (recorder-side).
pub(crate) fn take() -> ProbeData {
    cell().lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default()
}

/// Millisecond wall-clock as the integer the wire carries.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "clamped non-negative and bounded by u64::MAX ms (~584My) before the cast; precision above 2^52 ms is irrelevant for wall-clock"
)]
pub fn ms(value: f64) -> u64 {
    value.max(0.0).min(u64::MAX as f64).round() as u64
}

/// `8.4.7` → `8.4`; anything not digits-dot-digits yields `None`.
pub fn minor(version: &str) -> Option<String> {
    let mut parts = version.split('.');
    let major = parts.next()?;
    let minor = parts.next().unwrap_or("0");
    if major.is_empty() || !major.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if minor.is_empty() || !minor.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{major}.{minor}"))
}

/// Dist-cache hit share of a run's fetches, 0–100; `None` when
/// nothing needed fetching (an all-up-to-date sync says nothing about
/// the cache).
pub fn cache_hit_pct(hits: u64, downloads: u64) -> Option<u8> {
    let total = hits + downloads;
    (total > 0).then(|| u8::try_from(hits * 100 / total).unwrap_or(100))
}

/// Dependency-count bucket (TELEMETRY.md): raw counts are precise
/// enough to fingerprint a project, buckets aren't.
pub fn bucket(n: usize) -> &'static str {
    match n {
        0 => "0",
        1..=5 => "1-5",
        6..=15 => "6-15",
        16..=40 => "16-40",
        41..=100 => "41-100",
        _ => "100+",
    }
}

/// Lowercase, dedupe, sort, and keep only vocabulary members.
pub fn filter_vocab(names: impl IntoIterator<Item = String>, vocab: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = names
        .into_iter()
        .map(|n| n.trim().to_ascii_lowercase())
        .filter(|n| vocab.contains(&n.as_str()))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Pattern-gate a single token whose vocabulary lives server-side
/// (e.g. the PHP flavor, a closed set defined by the bougie index).
pub fn vocab_token(token: &str) -> Option<String> {
    let t = token.trim().to_ascii_lowercase();
    let ok = !t.is_empty()
        && t.len() <= 24
        && t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    ok.then_some(t)
}

/// True when the marker says the ecosystem set already shipped within
/// [`ECOSYSTEM_INTERVAL`].
pub(crate) fn ecosystem_fresh(marker: &Path) -> bool {
    std::fs::metadata(marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < ECOSYSTEM_INTERVAL)
}

pub(crate) fn touch_marker(marker: &Path) {
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(marker, b"");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minor_versions() {
        assert_eq!(minor("8.4.7").as_deref(), Some("8.4"));
        assert_eq!(minor("8.4").as_deref(), Some("8.4"));
        assert_eq!(minor("8").as_deref(), Some("8.0"));
        assert_eq!(minor("dev-main"), None);
        assert_eq!(minor("8.x"), None);
        assert_eq!(minor(""), None);
    }

    #[test]
    fn cache_hit_percentage() {
        assert_eq!(cache_hit_pct(0, 0), None, "nothing fetched, nothing to say");
        assert_eq!(cache_hit_pct(10, 0), Some(100));
        assert_eq!(cache_hit_pct(0, 10), Some(0));
        assert_eq!(cache_hit_pct(1, 2), Some(33));
    }

    #[test]
    fn buckets() {
        assert_eq!(bucket(0), "0");
        assert_eq!(bucket(5), "1-5");
        assert_eq!(bucket(6), "6-15");
        assert_eq!(bucket(40), "16-40");
        assert_eq!(bucket(100), "41-100");
        assert_eq!(bucket(101), "100+");
    }

    #[test]
    fn vocab_filtering_drops_unknown_and_dedupes() {
        let filtered = filter_vocab(
            ["Redis".into(), "gd".into(), "my-private-ext".into(), "redis".into()],
            EXTENSION_VOCAB,
        );
        assert_eq!(filtered, vec!["gd".to_owned(), "redis".to_owned()]);
    }

    #[test]
    fn vocab_token_gate() {
        assert_eq!(vocab_token("Standard").as_deref(), Some("standard"));
        assert_eq!(vocab_token("weird flavor!"), None);
        assert_eq!(vocab_token(""), None);
    }

    #[test]
    fn ecosystem_throttle_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join("projects/abc/telemetry-last-snapshot");
        assert!(!ecosystem_fresh(&marker));
        touch_marker(&marker);
        assert!(ecosystem_fresh(&marker));
    }

    #[test]
    fn record_and_take_round_trip() {
        record(|p| p.enrich.resolve_ms = Some(42));
        let data = take();
        assert_eq!(data.enrich.resolve_ms, Some(42));
        assert_eq!(take().enrich.resolve_ms, None, "take drains");
    }
}
