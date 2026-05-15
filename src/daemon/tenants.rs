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
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::io::AsyncWriteExt;

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
pub async fn load_all(path: &Path) -> Result<Vec<Tenant>> {
    let contents = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(eyre!("reading {}: {e}", path.display())),
    };
    let mut out = Vec::new();
    for (i, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let t: Tenant = serde_json::from_str(line)
            .map_err(|e| eyre!("parsing line {} of {}: {e}", i + 1, path.display()))?;
        out.push(t);
    }
    Ok(out)
}

/// Append a tenant record. fsync between write and close so a crash
/// after this call leaves the ledger consistent on disk.
pub async fn append(path: &Path, tenant: &Tenant) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    let mut json = serde_json::to_vec(tenant)
        .wrap_err("serialising tenant record")?;
    json.push(b'\n');
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .wrap_err_with(|| format!("opening {} for append", path.display()))?;
    f.write_all(&json)
        .await
        .wrap_err_with(|| format!("appending to {}", path.display()))?;
    f.sync_all()
        .await
        .wrap_err_with(|| format!("fsync {}", path.display()))?;
    Ok(())
}

/// Rewrite the ledger with the predicate-passing records only. Used
/// by `service.down` to remove a project's tenant. Atomic via
/// tempfile + rename.
pub async fn rewrite(path: &Path, keep: impl Fn(&Tenant) -> bool) -> Result<usize> {
    let all = load_all(path).await?;
    let kept: Vec<&Tenant> = all.iter().filter(|t| keep(t)).collect();
    let removed = all.len() - kept.len();
    if removed == 0 {
        return Ok(0);
    }
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("path {} has no parent", path.display()))?;
    let dir = if parent.as_os_str().is_empty() {
        Path::new(".").to_path_buf()
    } else {
        tokio::fs::create_dir_all(parent)
            .await
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
        parent.to_path_buf()
    };
    // Use a manual temp-file + rename instead of `tempfile::NamedTempFile`:
    // the latter is sync and owns a `std::fs::File`. The two
    // `tokio::fs::rename` consumers below would otherwise need a
    // `spawn_blocking` bridge — cheaper to just inline the pattern.
    let tmp_name = format!(
        "{}.tmp.{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "tenants".into()),
        std::process::id(),
    );
    let tmp_path = dir.join(tmp_name);
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp_path)
        .await
        .wrap_err_with(|| format!("creating tempfile {}", tmp_path.display()))?;
    for t in &kept {
        let bytes = serde_json::to_vec(t).wrap_err("serialising tenant on rewrite")?;
        f.write_all(&bytes).await.wrap_err("writing tempfile")?;
        f.write_all(b"\n").await.wrap_err("writing newline")?;
    }
    f.sync_all().await.wrap_err("fsync tempfile")?;
    drop(f);
    tokio::fs::rename(&tmp_path, path)
        .await
        .wrap_err_with(|| format!("renaming {} → {}", tmp_path.display(), path.display()))?;
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

    #[tokio::test]
    async fn load_all_handles_missing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        assert!(load_all(&path).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn append_then_load_roundtrips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        let mut t = Tenant::new("acme_blog", "/work/blog");
        t.alloc.insert("db_number".into(), serde_json::json!(3));
        append(&path, &t).await.unwrap();
        let loaded = load_all(&path).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].tenant, "acme_blog");
        assert_eq!(loaded[0].project, std::path::Path::new("/work/blog"));
        assert_eq!(loaded[0].alloc["db_number"], 3);
    }

    #[tokio::test]
    async fn append_two_records_preserves_order() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        append(&path, &Tenant::new("first", "/a")).await.unwrap();
        append(&path, &Tenant::new("second", "/b")).await.unwrap();
        let loaded = load_all(&path).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].tenant, "first");
        assert_eq!(loaded[1].tenant, "second");
    }

    #[tokio::test]
    async fn rewrite_drops_predicate_failures() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        append(&path, &Tenant::new("keep_me", "/a")).await.unwrap();
        append(&path, &Tenant::new("drop_me", "/b")).await.unwrap();
        append(&path, &Tenant::new("keep_me_too", "/c")).await.unwrap();
        let removed = rewrite(&path, |t| !t.tenant.starts_with("drop")).await.unwrap();
        assert_eq!(removed, 1);
        let loaded = load_all(&path).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].tenant, "keep_me");
        assert_eq!(loaded[1].tenant, "keep_me_too");
    }

    #[tokio::test]
    async fn rewrite_skips_io_when_nothing_to_remove() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        append(&path, &Tenant::new("keep", "/a")).await.unwrap();
        let removed = rewrite(&path, |_| true).await.unwrap();
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn malformed_line_surfaces_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        std::fs::write(&path, "not json\n").unwrap();
        assert!(load_all(&path).await.is_err());
    }

    #[test]
    fn civil_from_days_zero_is_epoch() {
        // 1970-01-01
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }
}
