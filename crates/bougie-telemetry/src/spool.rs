//! On-disk event spool: NDJSON, one file per UTC day, size/age-capped.
//!
//! Lives under the *cache* root — transient by definition, safe to wipe.
//! Appends are best-effort: a full disk or permission problem drops the
//! event, never the command.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Total spool cap; oldest files are pruned first once exceeded.
pub const MAX_TOTAL_BYTES: u64 = 1024 * 1024;
/// Days of spool to keep, compared lexically against `yyyy-mm-dd` names.
pub const MAX_AGE_DAYS: i64 = 30;

#[derive(Debug, Clone)]
pub struct Spool {
    dir: PathBuf,
}

impl Spool {
    /// Spool rooted under the given cache root
    /// (`<cache>/telemetry/spool/`).
    pub fn new(cache_root: &Path) -> Self {
        Self { dir: cache_root.join("telemetry").join("spool") }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append one NDJSON line to today's file. Best-effort by contract.
    pub fn append(&self, date: &str, line: &str) {
        if let Err(err) = self.try_append(date, line) {
            tracing::debug!("telemetry spool append failed: {err}");
        }
    }

    fn try_append(&self, date: &str, line: &str) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{date}.ndjson"));
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        drop(file);
        self.enforce_caps();
        Ok(())
    }

    /// Drop whole files, oldest first, until the spool fits the byte
    /// cap; drop anything older than [`MAX_AGE_DAYS`] outright. The
    /// newest file always survives (a single oversized day beats
    /// losing today's events).
    fn enforce_caps(&self) {
        let mut files = self.files();
        let cutoff = cutoff_date();
        while files.len() > 1 {
            let oldest = &files[0];
            let stale = oldest
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|d| d < cutoff.as_str());
            let total: u64 = files
                .iter()
                .map(|p| fs::metadata(p).map_or(0, |m| m.len()))
                .sum();
            if !stale && total <= MAX_TOTAL_BYTES {
                break;
            }
            let _ = fs::remove_file(oldest);
            files.remove(0);
        }
    }

    /// Spool files sorted oldest → newest (lexical == chronological for
    /// `yyyy-mm-dd` names).
    pub fn files(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "ndjson"))
            .collect();
        files.sort();
        files
    }

    pub fn total_bytes(&self) -> u64 {
        self.files()
            .iter()
            .map(|p| fs::metadata(p).map_or(0, |m| m.len()))
            .sum()
    }

    /// Date (`yyyy-mm-dd`) of the oldest spool file, if any.
    pub fn oldest_date(&self) -> Option<String> {
        self.files()
            .first()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .map(str::to_owned)
    }

    /// The last `limit` spooled lines, oldest → newest. `limit == 0`
    /// means all.
    pub fn last_lines(&self, limit: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for file in self.files() {
            if let Ok(contents) = fs::read_to_string(&file) {
                lines.extend(contents.lines().map(str::to_owned));
            }
        }
        if limit > 0 && lines.len() > limit {
            lines.drain(..lines.len() - limit);
        }
        lines
    }

    /// Total spooled event count.
    pub fn event_count(&self) -> usize {
        self.files()
            .iter()
            .filter_map(|f| fs::read_to_string(f).ok())
            .map(|c| c.lines().count())
            .sum()
    }

    /// Remove every spool file (and the dir, best-effort).
    pub fn purge(&self) {
        for file in self.files() {
            let _ = fs::remove_file(file);
        }
        let _ = fs::remove_dir(&self.dir);
    }
}

/// `now - MAX_AGE_DAYS`, rendered `yyyy-mm-dd`; spool-file names sort
/// lexically, so a plain string compare decides staleness.
fn cutoff_date() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    crate::clock::UtcHour::from_unix_seconds(now - MAX_AGE_DAYS * 86_400).date()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn append_and_read_back() {
        let tmp = TempDir::new().unwrap();
        let spool = Spool::new(tmp.path());
        spool.append("2026-07-03", r#"{"a":1}"#);
        spool.append("2026-07-03", r#"{"a":2}"#);
        assert_eq!(spool.event_count(), 2);
        assert_eq!(spool.last_lines(1), vec![r#"{"a":2}"#.to_owned()]);
        assert_eq!(spool.files().len(), 1);
    }

    #[test]
    fn size_cap_drops_oldest_file_first() {
        let tmp = TempDir::new().unwrap();
        let spool = Spool::new(tmp.path());
        let big = "x".repeat(600 * 1024);
        spool.append("2026-07-01", &big);
        spool.append("2026-07-02", &big);
        // Two ~600 KiB files exceed the 1 MiB cap → oldest pruned.
        assert_eq!(spool.files().len(), 1);
        assert!(spool.oldest_date().unwrap().ends_with("02"));
    }

    #[test]
    fn newest_file_survives_even_when_oversized() {
        let tmp = TempDir::new().unwrap();
        let spool = Spool::new(tmp.path());
        spool.append("2026-07-03", &"x".repeat(2 * 1024 * 1024));
        assert_eq!(spool.files().len(), 1);
    }

    #[test]
    fn stale_files_pruned_by_age() {
        let tmp = TempDir::new().unwrap();
        let spool = Spool::new(tmp.path());
        spool.append("1999-01-01", r#"{"old":true}"#);
        spool.append("2999-01-01", r#"{"new":true}"#);
        assert_eq!(spool.files().len(), 1);
        assert_eq!(spool.oldest_date().unwrap(), "2999-01-01");
    }

    #[test]
    fn purge_empties_spool() {
        let tmp = TempDir::new().unwrap();
        let spool = Spool::new(tmp.path());
        spool.append("2026-07-03", "{}");
        spool.purge();
        assert_eq!(spool.event_count(), 0);
        assert_eq!(spool.total_bytes(), 0);
    }
}
