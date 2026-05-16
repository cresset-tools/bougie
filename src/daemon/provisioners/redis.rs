//! Redis tenancy: logical DB number allocator. SERVICES.md §3.2.
//!
//! Redis supports 16 logical databases (0..15) addressable via
//! `SELECT <n>`. The provisioner reserves one per project tenant and
//! records the allocation in `tenants.json`. Hitting the 16-tenant cap
//! returns a structured error so the daemon can surface a clear hint.

use crate::daemon::tenants::{self, Tenant};
use eyre::{eyre, Result};
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;

/// Total DB count exposed by redis. Configurable in redis.conf, but
/// bougie's catalog ships defaults; v1 keeps the cap at 16.
const REDIS_DB_COUNT: u8 = 16;

/// Reserve the next free DB number for `tenant_name`. If the project
/// already has a tenant for this service, re-uses its allocation
/// (idempotent `services up`).
pub fn provision(tenants_path: &Path, tenant_name: &str, project: &Path) -> Result<Tenant> {
    let existing = tenants::load_all(tenants_path)?;

    // Idempotence: same project gets the same tenant back.
    if let Some(existing_t) = existing.iter().find(|t| t.project == project) {
        return Ok(existing_t.clone());
    }

    let taken: HashSet<u64> = existing
        .iter()
        .filter_map(|t| t.alloc.get("db_number").and_then(serde_json::Value::as_u64))
        .collect();

    let db_number = (0..u64::from(REDIS_DB_COUNT))
        .find(|n| !taken.contains(n))
        .ok_or_else(|| {
            eyre!(
                "redis: all {} logical DB slots are in use; remove an unused tenant \
                 with `bougie down redis --purge` from one of the holding projects",
                REDIS_DB_COUNT
            )
        })?;

    let mut tenant = Tenant::new(tenant_name, project.to_path_buf());
    tenant.alloc.insert("db_number".into(), json!(db_number));

    tenants::append(tenants_path, &tenant)?;
    Ok(tenant)
}

/// Release a tenant's DB-number reservation. With `purge`, also runs
/// `FLUSHDB` against the live redis socket so the keys are gone.
pub fn deprovision(
    tenants_path: &Path,
    tenant_name: &str,
    socket_path: Option<&Path>,
    purge: bool,
) -> Result<()> {
    let existing = tenants::load_all(tenants_path)?;
    let Some(target) = existing.iter().find(|t| t.tenant == tenant_name).cloned() else {
        return Ok(()); // nothing to do
    };
    if purge {
        if let Some(sock) = socket_path {
            if let Some(db) = target.alloc.get("db_number").and_then(serde_json::Value::as_u64) {
                flush_db(sock, db)?;
            }
        }
    }
    tenants::rewrite(tenants_path, |t| t.tenant != tenant_name)?;
    Ok(())
}

/// `redis-cli -s <sock> -n <db> FLUSHDB`. We don't ship a redis-cli;
/// instead, speak the resp protocol directly — `*2\r\n$6\r\nSELECT\r\n$N\r\n<n>\r\n*1\r\n$7\r\nFLUSHDB\r\n`.
fn flush_db(socket: &Path, db: u64) -> Result<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let mut s = UnixStream::connect(socket)
        .map_err(|e| eyre!("connecting to redis socket {}: {e}", socket.display()))?;
    let db_str = db.to_string();
    let cmd = format!(
        "*2\r\n$6\r\nSELECT\r\n${len}\r\n{db}\r\n*1\r\n$7\r\nFLUSHDB\r\n",
        len = db_str.len(),
        db = db_str
    );
    s.write_all(cmd.as_bytes())
        .map_err(|e| eyre!("sending FLUSHDB: {e}"))?;
    // Read enough to consume both replies (two "+OK\r\n" lines). Don't
    // care about the content beyond the connection acknowledging.
    let mut buf = [0u8; 64];
    let _ = s.read(&mut buf);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn first_tenant_gets_db_zero() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        let t = provision(&path, "acme_blog", Path::new("/work/blog")).unwrap();
        assert_eq!(t.alloc["db_number"], 0);
    }

    #[test]
    fn second_tenant_gets_next_db() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        provision(&path, "a", Path::new("/p/a")).unwrap();
        let b = provision(&path, "b", Path::new("/p/b")).unwrap();
        assert_eq!(b.alloc["db_number"], 1);
    }

    #[test]
    fn re_provision_same_project_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        let first = provision(&path, "a", Path::new("/p/a")).unwrap();
        let again = provision(&path, "a", Path::new("/p/a")).unwrap();
        assert_eq!(first.tenant, again.tenant);
        assert_eq!(first.alloc["db_number"], again.alloc["db_number"]);
        // No new line in the ledger.
        assert_eq!(tenants::load_all(&path).unwrap().len(), 1);
    }

    #[test]
    fn exhausted_db_slots_errors_with_hint() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        for i in 0..16 {
            provision(&path, &format!("p{i}"), Path::new(&format!("/p/{i}"))).unwrap();
        }
        let err = provision(&path, "overflow", Path::new("/p/over")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("16"));
        assert!(msg.contains("--purge"), "{msg}");
    }

    #[test]
    fn deprovision_drops_the_tenant_record() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        provision(&path, "a", Path::new("/p/a")).unwrap();
        provision(&path, "b", Path::new("/p/b")).unwrap();
        deprovision(&path, "a", None, false).unwrap();
        let remaining = tenants::load_all(&path).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].tenant, "b");
    }

    #[test]
    fn deprovision_freed_slot_can_be_reused() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        provision(&path, "a", Path::new("/p/a")).unwrap();
        provision(&path, "b", Path::new("/p/b")).unwrap();
        deprovision(&path, "a", None, false).unwrap();
        let c = provision(&path, "c", Path::new("/p/c")).unwrap();
        // a was DB 0 → now free → c gets 0.
        assert_eq!(c.alloc["db_number"], 0);
    }
}
