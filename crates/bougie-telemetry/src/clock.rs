//! Minimal UTC civil-date math over `SystemTime`.
//!
//! Telemetry needs exactly two renderings — a `yyyy-mm-dd` spool-file
//! date and an hour-truncated RFC 3339 event timestamp (sub-hour
//! resolution is deliberately never recorded) — which doesn't justify a
//! date-crate dependency. The days→civil conversion is Howard Hinnant's
//! `civil_from_days` algorithm.

use std::time::{SystemTime, UNIX_EPOCH};

/// A UTC instant truncated to the hour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcHour {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
}

impl UtcHour {
    /// Current UTC time, truncated to the hour.
    pub fn now() -> Self {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        Self::from_unix_seconds(secs)
    }

    pub fn from_unix_seconds(secs: i64) -> Self {
        let days = secs.div_euclid(86_400);
        let hour = u32::try_from(secs.rem_euclid(86_400) / 3_600).unwrap_or(0);
        let (year, month, day) = civil_from_days(days);
        Self { year, month, day, hour }
    }

    /// `2026-07-03` — the spool-file date. Lexical order == chronological
    /// order, which the spool's oldest-first pruning relies on.
    pub fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// `2026-07-03T09:00:00Z` — the hour-truncated event timestamp.
    pub fn rfc3339(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:00:00Z",
            self.year, self.month, self.day, self.hour
        )
    }
}

/// `2026-07-09 06:12:34 UTC` from unix seconds — full precision, for
/// human-facing listings of *local* artifacts (the failure ring).
/// Telemetry events themselves stay hour-truncated by construction;
/// nothing on the wire uses this.
pub fn format_epoch_utc(secs: u64) -> String {
    let secs = i64::try_from(secs).unwrap_or(0);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Days since 1970-01-01 → (year, month, day). Hinnant's algorithm;
/// exact for the entire i64-day range we can encounter.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Hinnant civil-from-days: every narrowed value is range-proven by the algorithm ([0,146096], [0,399], [1,31], [1,12])"
)]
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_1970() {
        let t = UtcHour::from_unix_seconds(0);
        assert_eq!(t.date(), "1970-01-01");
        assert_eq!(t.rfc3339(), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_instant() {
        // 2026-07-03 09:41:27 UTC == 1783071687
        // (epoch day 20637 = 20454 for 1970→2026-01-01 + 183 into 2026).
        let t = UtcHour::from_unix_seconds(1_783_071_687);
        assert_eq!(t.date(), "2026-07-03");
        assert_eq!(t.rfc3339(), "2026-07-03T09:00:00Z");
    }

    #[test]
    fn leap_day() {
        // 2024-02-29 23:59:59 UTC == 1709251199.
        let t = UtcHour::from_unix_seconds(1_709_251_199);
        assert_eq!(t.date(), "2024-02-29");
        assert_eq!(t.rfc3339(), "2024-02-29T23:00:00Z");
    }

    #[test]
    fn pre_epoch() {
        // 1969-12-31 23:00:00 UTC == -3600.
        let t = UtcHour::from_unix_seconds(-3_600);
        assert_eq!(t.rfc3339(), "1969-12-31T23:00:00Z");
    }
}
