//! Per-request log emission. Spec: SERVER.md §9.
//!
//! `text` mode: one ANSI-coloured line per request on stderr.
//! `json-v1` mode: NDJSON line per request on stderr.
//!
//! Background events (`pool_start`, `pool_idle_out`, etc.) ride the
//! same schema and are emitted by phase-2+ code calling
//! [`emit_event`].

use serde::Serialize;
use std::io::{self, Write};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    JsonV1,
}

impl LogFormat {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "text" => Ok(Self::Text),
            "json-v1" => Ok(Self::JsonV1),
            other => Err(format!("unknown log format: {other:?} (expected text|json-v1)")),
        }
    }
}

static FORMAT: OnceLock<LogFormat> = OnceLock::new();

pub fn init(format: LogFormat) {
    let _ = FORMAT.set(format);
}

fn format() -> LogFormat {
    *FORMAT.get().unwrap_or(&LogFormat::Text)
}

/// Per-request log row. Fields that don't apply to a given request
/// (e.g. `pool` and `php_version` on a static-file hit in phase 1) are
/// elided rather than emitted as `null` so the JSON schema stays
/// forward-compatible without a phase-1 floor of nullables.
#[derive(Debug, Serialize)]
pub struct RequestRow<'a> {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub ts: String,
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub status: u16,
    pub bytes_out: u64,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub php_version: Option<&'a str>,
}

impl<'a> RequestRow<'a> {
    pub fn new(
        method: &'a str,
        host: &'a str,
        path: &'a str,
        status: u16,
        bytes_out: u64,
        duration_ms: u64,
    ) -> Self {
        Self {
            schema_version: 1,
            kind: "request",
            ts: rfc3339_now(),
            method,
            host,
            path,
            status,
            bytes_out,
            duration_ms,
            pool: None,
            project: None,
            php_version: None,
        }
    }

    #[must_use]
    pub fn with_pool(mut self, pool: &'a str) -> Self {
        self.pool = Some(pool);
        self
    }

    #[must_use]
    pub fn with_project(mut self, project: &'a str) -> Self {
        self.project = Some(project);
        self
    }

    #[must_use]
    pub fn with_php_version(mut self, version: &'a str) -> Self {
        self.php_version = Some(version);
        self
    }
}

pub fn emit_request(row: &RequestRow<'_>) {
    let stderr = io::stderr();
    let mut w = stderr.lock();
    let _ = match format() {
        LogFormat::Text => write_request_text(&mut w, row),
        LogFormat::JsonV1 => write_event_json(&mut w, row),
    };
}

fn write_request_text<W: Write>(w: &mut W, r: &RequestRow<'_>) -> io::Result<()> {
    // Stay close to nginx-ish access log shape so the line is at-a-glance
    // scannable in dev. ANSI is intentionally off here — `bougie server`
    // already uses `--format` to colour stdout; stderr stays plain to
    // play nicely with redirection.
    writeln!(
        w,
        "{} {} {} {} -> {} ({} bytes, {}ms)",
        r.ts, r.method, r.host, r.path, r.status, r.bytes_out, r.duration_ms
    )
}

fn write_event_json<W: Write, T: Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    serde_json::to_writer(&mut *w, value).map_err(io::Error::other)?;
    writeln!(w)
}

/// Background event (`pool_start`, `pool_reload`, ...). Phase-2+ uses this.
#[allow(dead_code)]
pub fn emit_event<T: Serialize + std::fmt::Debug>(event: &T) {
    let stderr = io::stderr();
    let mut w = stderr.lock();
    match format() {
        LogFormat::Text => {
            // Best-effort text rendering: re-serialize to JSON then
            // print as a single line. Phase-2 lifecycle events have
            // varied shapes; we don't try to format each one specially.
            if let Ok(s) = serde_json::to_string(event) {
                let _ = writeln!(w, "{s}");
            }
        }
        LogFormat::JsonV1 => {
            let _ = write_event_json(&mut w, event);
        }
    }
}

fn rfc3339_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let millis = now.subsec_millis();
    let (year, month, day, hour, min, sec) = epoch_to_components(total_secs);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z"
    )
}

/// Mini Gregorian decomposition. Pulled inline rather than adding a
/// `chrono` / `time` dep purely for logging timestamps. Covers 1970–9999.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn epoch_to_components(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;
    let (year, month, day) = days_to_ymd(days);
    (year, month, day, hour, min, sec)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn days_to_ymd(mut days: i64) -> (u32, u32, u32) {
    // Algorithm from Howard Hinnant's date library, simplified for
    // post-1970 dates.
    days += 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp.wrapping_sub(9) };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_format_round_trip() {
        assert_eq!(LogFormat::parse("text").unwrap(), LogFormat::Text);
        assert_eq!(LogFormat::parse("json-v1").unwrap(), LogFormat::JsonV1);
        assert!(LogFormat::parse("yaml").is_err());
    }

    #[test]
    fn request_row_serializes_minimally() {
        let row = RequestRow::new("GET", "x.bougie.run", "/", 200, 1234, 5);
        let s = serde_json::to_string(&row).unwrap();
        assert!(s.contains("\"schema_version\":1"));
        assert!(s.contains("\"type\":\"request\""));
        assert!(s.contains("\"status\":200"));
        // Optional fields elided when absent.
        assert!(!s.contains("\"pool\""));
        assert!(!s.contains("\"php_version\""));
    }

    #[test]
    fn request_row_with_pool_emits_field() {
        let row = RequestRow::new("GET", "x", "/", 200, 0, 1).with_pool("normal");
        let s = serde_json::to_string(&row).unwrap();
        assert!(s.contains("\"pool\":\"normal\""));
    }

    #[test]
    fn epoch_zero_is_1970_01_01() {
        assert_eq!(epoch_to_components(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn one_day_lands_on_jan_2() {
        let (y, mo, d, h, mi, s) = epoch_to_components(86_400);
        assert_eq!((y, mo, d, h, mi, s), (1970, 1, 2, 0, 0, 0));
    }

    #[test]
    fn one_year_non_leap_lands_on_1971_01_01() {
        let (y, mo, d, h, mi, s) = epoch_to_components(86_400 * 365);
        assert_eq!((y, mo, d, h, mi, s), (1971, 1, 1, 0, 0, 0));
    }

    #[test]
    fn jan_31_decomposes() {
        let (y, mo, d, _, _, _) = epoch_to_components(86_400 * 30);
        assert_eq!((y, mo, d), (1970, 1, 31));
    }
}
