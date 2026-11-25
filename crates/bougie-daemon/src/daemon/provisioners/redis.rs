//! Redis tenancy: logical DB number allocator. SERVICES.md §3.2.
//!
//! Redis addresses logical databases with `SELECT <n>`. bougied starts
//! redis with [`REDIS_DATABASES`] of them (redis's own default is 16,
//! which one developer's projects outgrow), reserves one per project
//! tenant and records the allocation in `tenants.json`. The allocator
//! asks the running redis how many it has, so an instance started before
//! the cap was raised keeps handing out only the numbers it can serve.
//! Running out returns a structured error so the daemon can surface a
//! clear hint.

use crate::daemon::tenants::{self, Tenant};
use eyre::{Result, eyre};
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How many logical databases bougied starts redis with (`--databases`).
/// redis reserves about 6 KB per database up front, empty or not: 4096
/// costs about 25 MB (measured on redis 8.6), and no developer runs out.
pub(crate) const REDIS_DATABASES: u64 = 4096;

/// redis's built-in default, assumed when the running instance can't be
/// asked (it predates `--databases`, or the socket doesn't answer).
const REDIS_DEFAULT_DATABASES: u64 = 16;

/// Serialises allocation: two `service up` requests for different
/// projects must not pick the same free number, and two for the same
/// project must not both append a tenant. The ledger is append-only and
/// lock-free, so the read-pick-append sequence needs this around it.
static ALLOCATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Reserve the next free DB number below `db_count` for `tenant_name`.
/// If the project already has a tenant for this service, re-uses its
/// allocation (idempotent `service up`).
pub async fn provision(
    tenants_path: &Path,
    tenant_name: &str,
    project: &Path,
    db_count: u64,
) -> Result<Tenant> {
    let _guard = ALLOCATE.lock().await;
    let existing = tenants::load_all(tenants_path).await?;

    // Idempotence: same project gets the same tenant back.
    if let Some(existing_t) = existing.iter().find(|t| t.project == project) {
        return Ok(existing_t.clone());
    }

    let taken: HashSet<u64> = existing
        .iter()
        .filter_map(|t| t.alloc.get("db_number").and_then(serde_json::Value::as_u64))
        .collect();

    let db_number = (0..db_count).find(|n| !taken.contains(n)).ok_or_else(|| {
        let restart = if db_count < REDIS_DATABASES {
            format!(
                "; this redis was started with {db_count}, restart it (`bougie service restart redis`) \
                 for {REDIS_DATABASES}"
            )
        } else {
            String::new()
        };
        eyre!(
            "redis: all {db_count} logical DB slots are in use{restart}; remove an unused tenant \
             with `bougie down redis --purge` from one of the holding projects"
        )
    })?;

    let mut tenant = Tenant::new(tenant_name, project.to_path_buf());
    tenant.alloc.insert("db_number".into(), json!(db_number));

    tenants::append(tenants_path, &tenant).await?;
    Ok(tenant)
}

/// Release a tenant's DB-number reservation. With `purge`, also runs
/// `FLUSHDB` against the live redis socket so the keys are gone.
pub async fn deprovision(
    tenants_path: &Path,
    tenant_name: &str,
    socket_path: Option<&Path>,
    purge: bool,
) -> Result<()> {
    let existing = tenants::load_all(tenants_path).await?;
    let Some(target) = existing.iter().find(|t| t.tenant == tenant_name).cloned() else {
        return Ok(()); // nothing to do
    };
    if purge
        && let Some(sock) = socket_path
        && let Some(db) = target
            .alloc
            .get("db_number")
            .and_then(serde_json::Value::as_u64)
    {
        flush_db(sock, db).await?;
    }
    tenants::rewrite(tenants_path, |t| t.tenant != tenant_name).await?;
    Ok(())
}

/// Health probe: `PING` over the redis socket, expect `+PONG`. We ship
/// no `redis-cli`, so — like [`flush_db`] — we speak RESP directly. This
/// is a real liveness check: it proves redis is accepting *and answering*
/// commands, not just that the socket is bound (which a bare connect, the
/// supervisor's old probe, can't distinguish).
pub(crate) async fn health(socket: &Path) -> Result<()> {
    let mut s = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| eyre!("connecting to redis socket {}: {e}", socket.display()))?;
    // Inline `PING` command. `+PONG\r\n` is the success reply.
    s.write_all(b"*1\r\n$4\r\nPING\r\n")
        .await
        .map_err(|e| eyre!("sending PING: {e}"))?;
    let mut buf = [0u8; 32];
    let n = s
        .read(&mut buf)
        .await
        .map_err(|e| eyre!("reading PING reply: {e}"))?;
    if buf[..n].starts_with(b"+PONG") {
        Ok(())
    } else {
        Err(eyre!(
            "redis PING returned an unexpected reply: {:?}",
            String::from_utf8_lossy(&buf[..n])
        ))
    }
}

/// `CONFIG GET databases` on the running instance, or redis's default
/// when it can't be asked within a couple of seconds.
pub(crate) async fn database_count(socket: &Path) -> u64 {
    let ask = async {
        let mut s = tokio::net::UnixStream::connect(socket).await.ok()?;
        s.write_all(b"*3\r\n$6\r\nCONFIG\r\n$3\r\nGET\r\n$9\r\ndatabases\r\n")
            .await
            .ok()?;
        let mut buf = [0u8; 128];
        let n = s.read(&mut buf).await.ok()?;
        parse_config_get_count(&buf[..n])
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), ask)
        .await
        .ok()
        .flatten()
        .unwrap_or(REDIS_DEFAULT_DATABASES)
}

/// The value of a `CONFIG GET databases` reply:
/// `*2\r\n$9\r\ndatabases\r\n$3\r\n256\r\n`.
fn parse_config_get_count(reply: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(reply).ok()?;
    let mut lines = text.split("\r\n");
    if lines.next()? != "*2" {
        return None;
    }
    let _ = lines.next()?; // $9
    if !lines.next()?.eq_ignore_ascii_case("databases") {
        return None;
    }
    let _ = lines.next()?; // $<len>
    lines.next()?.parse().ok().filter(|&n| n > 0)
}

/// `redis-cli -s <sock> -n <db> FLUSHDB`. We don't ship a redis-cli;
/// instead, speak the resp protocol directly — `*2\r\n$6\r\nSELECT\r\n$N\r\n<n>\r\n*1\r\n$7\r\nFLUSHDB\r\n`.
async fn flush_db(socket: &Path, db: u64) -> Result<()> {
    let mut s = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| eyre!("connecting to redis socket {}: {e}", socket.display()))?;
    let db_str = db.to_string();
    let cmd = format!(
        "*2\r\n$6\r\nSELECT\r\n${len}\r\n{db}\r\n*1\r\n$7\r\nFLUSHDB\r\n",
        len = db_str.len(),
        db = db_str
    );
    s.write_all(cmd.as_bytes())
        .await
        .map_err(|e| eyre!("sending FLUSHDB: {e}"))?;
    // Read enough to consume both replies (two "+OK\r\n" lines). Don't
    // care about the content beyond the connection acknowledging.
    let mut buf = [0u8; 64];
    let _ = s.read(&mut buf).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn first_tenant_gets_db_zero() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        let t = provision(&path, "acme_blog", Path::new("/work/blog"), 16)
            .await
            .unwrap();
        assert_eq!(t.alloc["db_number"], 0);
    }

    #[tokio::test]
    async fn second_tenant_gets_next_db() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        provision(&path, "a", Path::new("/p/a"), 16).await.unwrap();
        let b = provision(&path, "b", Path::new("/p/b"), 16).await.unwrap();
        assert_eq!(b.alloc["db_number"], 1);
    }

    #[tokio::test]
    async fn re_provision_same_project_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        let first = provision(&path, "a", Path::new("/p/a"), 16).await.unwrap();
        let again = provision(&path, "a", Path::new("/p/a"), 16).await.unwrap();
        assert_eq!(first.tenant, again.tenant);
        assert_eq!(first.alloc["db_number"], again.alloc["db_number"]);
        // No new line in the ledger.
        assert_eq!(tenants::load_all(&path).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn exhausted_db_slots_errors_with_hint() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        for i in 0..16 {
            provision(&path, &format!("p{i}"), Path::new(&format!("/p/{i}")), 16)
                .await
                .unwrap();
        }
        let err = provision(&path, "overflow", Path::new("/p/over"), 16)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("16"));
        assert!(msg.contains("--purge"), "{msg}");
    }

    #[tokio::test]
    async fn deprovision_drops_the_tenant_record() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        provision(&path, "a", Path::new("/p/a"), 16).await.unwrap();
        provision(&path, "b", Path::new("/p/b"), 16).await.unwrap();
        deprovision(&path, "a", None, false).await.unwrap();
        let remaining = tenants::load_all(&path).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].tenant, "b");
    }

    #[tokio::test]
    async fn deprovision_freed_slot_can_be_reused() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        provision(&path, "a", Path::new("/p/a"), 16).await.unwrap();
        provision(&path, "b", Path::new("/p/b"), 16).await.unwrap();
        deprovision(&path, "a", None, false).await.unwrap();
        let c = provision(&path, "c", Path::new("/p/c"), 16).await.unwrap();
        // a was DB 0 → now free → c gets 0.
        assert_eq!(c.alloc["db_number"], 0);
    }

    #[tokio::test]
    async fn the_cap_follows_the_running_instance() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        for i in 0..16 {
            provision(
                &path,
                &format!("p{i}"),
                Path::new(&format!("/p/{i}")),
                REDIS_DATABASES,
            )
            .await
            .unwrap();
        }
        let t = provision(&path, "seventeenth", Path::new("/p/17"), REDIS_DATABASES)
            .await
            .unwrap();
        assert_eq!(t.alloc["db_number"], 16);
        // An instance still running with 16 can't serve db 17: the error says to restart it.
        let msg = format!(
            "{:#}",
            provision(&path, "x", Path::new("/p/x"), 16)
                .await
                .unwrap_err()
        );
        assert!(msg.contains("restart"), "{msg}");
    }

    /// Concurrent `service up`s used to read the ledger, pick, and append
    /// independently: the same project got two tenants, and two projects
    /// could get one number.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_provisioning_neither_duplicates_nor_collides() {
        let dir = TempDir::new().unwrap();
        let path = std::sync::Arc::new(dir.path().join("tenants.json"));
        let mut tasks = Vec::new();
        for i in 0..24 {
            let path = path.clone();
            // Every project asks twice.
            let project = format!("/p/{}", i % 12);
            tasks.push(tokio::spawn(async move {
                provision(
                    &path,
                    &format!("t{}", i % 12),
                    Path::new(&project),
                    REDIS_DATABASES,
                )
                .await
                .unwrap()
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let all = tenants::load_all(&path).await.unwrap();
        assert_eq!(all.len(), 12, "one tenant per project");
        let numbers: HashSet<u64> = all
            .iter()
            .map(|t| t.alloc["db_number"].as_u64().unwrap())
            .collect();
        assert_eq!(numbers.len(), 12, "no number handed out twice");
    }

    #[test]
    fn config_get_reply_is_parsed() {
        assert_eq!(
            parse_config_get_count(b"*2\r\n$9\r\ndatabases\r\n$3\r\n256\r\n"),
            Some(256)
        );
        assert_eq!(parse_config_get_count(b"+OK\r\n"), None);
    }
}
