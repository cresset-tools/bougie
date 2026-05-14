//! Per-service tenant ledger (SERVICES.md §3.3).
//!
//! `$BOUGIE_HOME/state/services/<svc>/tenants.json` is **JSON Lines**:
//! one record per line, appended atomically with fsync. Append-only
//! lets two concurrent `service.up` provisioning paths write side by
//! side without rewriting the file. Removals (the rare path) rewrite
//! the file using the same tempfile-then-rename pattern bougie's
//! `composer::lockfile` already uses.

use eyre::{eyre, Result, WrapErr};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One tenant entry. Schema-versioned for forward compat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    pub schema_version: u32,
    /// Public name visible inside the service (DB name, vhost, etc.).
    pub tenant: String,
    /// Absolute path of the project this tenant belongs to.
    pub project: PathBuf,
    /// RFC 3339 timestamp captured at provisioning.
    pub created_at: String,
    /// Generated secrets (e.g. mariadb password). Persisted because
    /// the service has no other way to recover them; users who care
    /// about rotation can `--purge` and re-provision.
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
    /// Provisioner-specific allocations: `{"db_number": 3}` for redis,
    /// `{"vhost": "myapp"}` for rabbitmq, etc.
    #[serde(default)]
    pub alloc: BTreeMap<String, Value>,
}

impl Tenant {
    pub fn new(tenant: impl Into<String>, project: impl Into<PathBuf>) -> Self {
        Self {
            schema_version: 1,
            tenant: tenant.into(),
            project: project.into(),
            created_at: now_rfc3339(),
            secrets: BTreeMap::new(),
            alloc: BTreeMap::new(),
        }
    }
}

/// Load every tenant from disk in insertion order. Lines that fail to
/// parse are returned as an error rather than skipped — silent skips
/// would mask a corruption that should surface to the operator.
pub fn load_all(path: &Path) -> Result<Vec<Tenant>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(path)
        .wrap_err_with(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line.wrap_err_with(|| format!("reading line {} of {}", i + 1, path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let t: Tenant = serde_json::from_str(&line)
            .map_err(|e| eyre!("parsing line {} of {}: {e}", i + 1, path.display()))?;
        out.push(t);
    }
    Ok(out)
}

/// Append a tenant record. fsync between write and close so a crash
/// after this call leaves the ledger consistent on disk.
pub fn append(path: &Path, tenant: &Tenant) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    let mut json = serde_json::to_vec(tenant)
        .wrap_err("serialising tenant record")?;
    json.push(b'\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .wrap_err_with(|| format!("opening {} for append", path.display()))?;
    f.write_all(&json)
        .wrap_err_with(|| format!("appending to {}", path.display()))?;
    f.sync_all()
        .wrap_err_with(|| format!("fsync {}", path.display()))?;
    Ok(())
}

/// Rewrite the ledger with the predicate-passing records only. Used
/// by `service.down` to remove a project's tenant. Atomic via
/// tempfile + rename.
pub fn rewrite(path: &Path, keep: impl Fn(&Tenant) -> bool) -> Result<usize> {
    let all = load_all(path)?;
    let kept: Vec<&Tenant> = all.iter().filter(|t| keep(t)).collect();
    let removed = all.len() - kept.len();
    if removed == 0 {
        return Ok(0);
    }
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("path {} has no parent", path.display()))?;
    let dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
        parent
    };
    let mut tf = tempfile::NamedTempFile::new_in(dir)
        .wrap_err_with(|| format!("creating tempfile in {}", dir.display()))?;
    for t in &kept {
        let bytes = serde_json::to_vec(t).wrap_err("serialising tenant on rewrite")?;
        tf.as_file_mut().write_all(&bytes).wrap_err("writing tempfile")?;
        tf.as_file_mut().write_all(b"\n").wrap_err("writing newline")?;
    }
    tf.as_file_mut().sync_all().wrap_err("fsync tempfile")?;
    tf.persist(path)
        .map_err(|e| eyre!("renaming temp to {}: {e}", path.display()))?;
    Ok(removed)
}

fn now_rfc3339() -> String {
    // Minimal RFC 3339 formatter — bougie doesn't pull in `chrono`,
    // and SystemTime → Duration → seconds-since-epoch is enough for
    // our audit/diagnostic use. Format: "1970-01-01T00:00:00Z" style.
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = unix_to_ymdhms(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}-{second:02}Z")
}

/// Strip-down UTC calendar arithmetic. `time` / `chrono` would do
/// this but bougie deliberately avoids the dep — the daemon needs
/// timestamps for audit, not full date math.
fn unix_to_ymdhms(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days_since_epoch = secs / 86_400;
    let seconds_today = secs % 86_400;
    let hour = (seconds_today / 3600) as u32;
    let minute = ((seconds_today % 3600) / 60) as u32;
    let second = (seconds_today % 60) as u32;
    let (year, month, day) = civil_from_days(days_since_epoch as i64);
    (year, month, day, hour, minute, second)
}

/// Howard Hinnant's `civil_from_days` algorithm. Converts a count of
/// days since 1970-01-01 into a (year, month, day) tuple.
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // 0..=146096
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy as u32 - ((153 * mp + 2) / 5) as u32 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_all_handles_missing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        assert!(load_all(&path).unwrap().is_empty());
    }

    #[test]
    fn append_then_load_roundtrips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        let mut t = Tenant::new("acme_blog", "/work/blog");
        t.alloc.insert("db_number".into(), serde_json::json!(3));
        append(&path, &t).unwrap();
        let loaded = load_all(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].tenant, "acme_blog");
        assert_eq!(loaded[0].project, std::path::Path::new("/work/blog"));
        assert_eq!(loaded[0].alloc["db_number"], 3);
    }

    #[test]
    fn append_two_records_preserves_order() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        append(&path, &Tenant::new("first", "/a")).unwrap();
        append(&path, &Tenant::new("second", "/b")).unwrap();
        let loaded = load_all(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].tenant, "first");
        assert_eq!(loaded[1].tenant, "second");
    }

    #[test]
    fn rewrite_drops_predicate_failures() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        append(&path, &Tenant::new("keep_me", "/a")).unwrap();
        append(&path, &Tenant::new("drop_me", "/b")).unwrap();
        append(&path, &Tenant::new("keep_me_too", "/c")).unwrap();
        let removed = rewrite(&path, |t| !t.tenant.starts_with("drop")).unwrap();
        assert_eq!(removed, 1);
        let loaded = load_all(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].tenant, "keep_me");
        assert_eq!(loaded[1].tenant, "keep_me_too");
    }

    #[test]
    fn rewrite_skips_io_when_nothing_to_remove() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        append(&path, &Tenant::new("keep", "/a")).unwrap();
        let removed = rewrite(&path, |_| true).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn malformed_line_surfaces_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        std::fs::write(&path, "not json\n").unwrap();
        assert!(load_all(&path).is_err());
    }

    #[test]
    fn civil_from_days_zero_is_epoch() {
        // 1970-01-01
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }
}
