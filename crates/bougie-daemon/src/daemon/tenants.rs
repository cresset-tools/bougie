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
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(eyre!("opening {}: {e}", path.display())),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| eyre!("decoding {} as UTF-8: {e}", path.display()))?;
    parse_ledger(text, path)
}

/// Synchronous twin of [`load_all`] for the daemon-less CLI paths
/// (`bougie projects list`, the service-client exec wiring) that read
/// the ledger without a tokio runtime.
pub fn load_all_sync(path: &Path) -> Result<Vec<Tenant>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(eyre!("opening {}: {e}", path.display())),
    };
    parse_ledger(&text, path)
}

/// The instance versions of a service that carry a tenant ledger on
/// disk, discovered by walking `name_dir` (`service_name_dir(name)` =
/// `state/services/<name>/`). Returns the `<version>` segment of every
/// child dir that holds a `tenants.json`.
///
/// This is the offline resolution seam (`INSTANCES_PLAN` §6): a service
/// that runs as two versions at once (`mysql` 8.0 beside 8.4) keeps a
/// separate ledger per version dir, so the daemon-less consumers
/// (`bougie run` env, `service credentials`, the client-exec wiring)
/// find *this* project's instance by scanning every version's ledger
/// rather than assuming the catalog default. Encodes "one version per
/// project per service": callers stop at the first version whose ledger
/// owns the project.
///
/// The `server` singleton keeps its ledger name-only
/// (`state/services/server/tenants.json`, no version segment — see
/// [`Paths::service_dir`]); that layout surfaces here as an empty-string
/// version, which round-trips back through `service_tenants(name, "")`
/// to the same name-only path. Order is unspecified; a missing dir
/// yields an empty vec.
///
/// [`Paths::service_dir`]: bougie_paths::Paths::service_dir
/// The instance version `project` runs for `service`, found by scanning
/// every on-disk ledger (`INSTANCES_PLAN` §6). Matches the project by
/// canonical path first, raw path second (the ledger stores whatever
/// spelling the daemon was handed). `None` when no instance's ledger
/// owns the project. The daemon-less display/IDE consumers (`diagnose`,
/// `PhpStorm` datasource) use this to point at the *right* version dir
/// rather than the catalog default.
#[must_use]
pub fn project_instance_version(
    paths: &bougie_paths::Paths,
    service: &str,
    project: &Path,
) -> Option<String> {
    let canon = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    for v in instance_versions(&paths.service_name_dir(service)) {
        if let Ok(rows) = load_all_sync(&paths.service_tenants(service, &v))
            && rows.iter().any(|t| t.project == canon || t.project == project)
        {
            return Some(v);
        }
    }
    None
}

#[must_use]
pub fn instance_versions(name_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    // Name-only ledger sitting directly under the service dir (server).
    if name_dir.join("tenants.json").is_file() {
        out.push(String::new());
    }
    let Ok(entries) = std::fs::read_dir(name_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        if entry.path().join("tenants.json").is_file()
            && let Some(v) = entry.file_name().to_str()
        {
            out.push(v.to_owned());
        }
    }
    out
}

fn parse_ledger(text: &str, path: &Path) -> Result<Vec<Tenant>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
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
/// write-to-temp-then-rename.
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
        Path::new(".")
    } else {
        tokio::fs::create_dir_all(parent)
            .await
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
        parent
    };
    let file_name = path
        .file_name()
        .ok_or_else(|| eyre!("path {} has no file name", path.display()))?;
    // `<file>.tmp.<pid>.<nanos>` — unique enough that two concurrent
    // rewrites against the same ledger don't collide, and we still
    // end up in the same directory so the final rename is atomic on
    // the same filesystem.
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp_name = format!(
        "{}.tmp.{}.{nanos}",
        file_name.to_string_lossy(),
        std::process::id()
    );
    let tmp_path = dir.join(&tmp_name);

    let mut buf = Vec::with_capacity(256 * kept.len());
    for t in &kept {
        serde_json::to_writer(&mut buf, t).wrap_err("serialising tenant on rewrite")?;
        buf.push(b'\n');
    }
    let mut tf = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp_path)
        .await
        .wrap_err_with(|| format!("creating tempfile {}", tmp_path.display()))?;
    if let Err(e) = tf.write_all(&buf).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(eyre!("writing tempfile {}: {e}", tmp_path.display()));
    }
    if let Err(e) = tf.sync_all().await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(eyre!("fsync tempfile {}: {e}", tmp_path.display()));
    }
    drop(tf);
    if let Err(e) = tokio::fs::rename(&tmp_path, path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(eyre!("renaming {} → {}: {e}", tmp_path.display(), path.display()));
    }
    Ok(removed)
}

fn now_rfc3339() -> String {
    // Minimal RFC 3339 formatter — bougie doesn't pull in `chrono`,
    // and SystemTime → Duration → seconds-since-epoch is enough for
    // our audit/diagnostic use. Format: "1970-01-01T00:00:00Z" style.
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (year, month, day, hour, minute, second) = unix_to_ymdhms(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Strip-down UTC calendar arithmetic. `time` / `chrono` would do
/// this but bougie deliberately avoids the dep — the daemon needs
/// timestamps for audit, not full date math.
fn unix_to_ymdhms(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days_since_epoch = secs / 86_400;
    let seconds_today = secs % 86_400;
    // `seconds_today < 86400` and `days_since_epoch < i64::MAX / 86400`
    // for any plausible Unix timestamp, so all the casts here are
    // lossless by construction.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "seconds_today < 86400; days_since_epoch fits in i64"
    )]
    {
        let hour = (seconds_today / 3600) as u32;
        let minute = ((seconds_today % 3600) / 60) as u32;
        let second = (seconds_today % 60) as u32;
        let (year, month, day) = civil_from_days(days_since_epoch as i64);
        (year, month, day, hour, minute, second)
    }
}

/// Howard Hinnant's `civil_from_days` algorithm. Converts a count of
/// days since 1970-01-01 into a (year, month, day) tuple. The algorithm
/// is published with these exact signed/unsigned conversions; the
/// intermediate values stay in well-defined ranges (proven in the
/// source paper) so the casts are correct by construction even though
/// they look sketchy. See
/// <http://howardhinnant.github.io/date_algorithms.html#civil_from_days>.
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    reason = "Hinnant's reference algorithm; intermediates stay in proven bounds"
)]
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
        tokio::fs::write(&path, "not json\n").await.unwrap();
        assert!(load_all(&path).await.is_err());
    }

    #[test]
    fn civil_from_days_zero_is_epoch() {
        // 1970-01-01
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }
}
