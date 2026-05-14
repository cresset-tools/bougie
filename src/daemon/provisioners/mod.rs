//! Per-service multi-tenancy provisioners.
//!
//! Phase 3 ships the redis allocator (DB number 0..15). mariadb /
//! opensearch / rabbitmq / bougie-server provisioners land in later
//! phases — the catalog already enumerates them via `Tenancy`.

pub mod redis;

use super::catalog::{CatalogEntry, Tenancy};
use super::tenants::Tenant;
use eyre::{eyre, Result};
use std::path::Path;

/// Dispatch a `provision` call to the right per-service implementation.
/// Returns a `Tenant` ready to be appended to `tenants.json`. The
/// caller is responsible for the append.
pub fn provision(
    entry: &CatalogEntry,
    tenants_path: &Path,
    tenant_name: &str,
    project: &Path,
) -> Result<Tenant> {
    match entry.tenancy {
        Tenancy::Redis => redis::provision(tenants_path, tenant_name, project),
        Tenancy::Mariadb
        | Tenancy::Opensearch
        | Tenancy::Rabbitmq
        | Tenancy::BougieServer => Err(eyre!(
            "{} provisioner not yet implemented (Phase 3 covers redis only)",
            entry.name
        )),
        Tenancy::None => Err(eyre!(
            "{} has no user-facing tenancy",
            entry.name
        )),
    }
}

/// Inverse of `provision` — symmetric dispatch. The redis path is
/// no-op unless `purge` is true, in which case it issues `FLUSHDB`
/// against the live socket.
pub fn deprovision(
    entry: &CatalogEntry,
    tenants_path: &Path,
    tenant_name: &str,
    socket_path: Option<&Path>,
    purge: bool,
) -> Result<()> {
    match entry.tenancy {
        Tenancy::Redis => redis::deprovision(tenants_path, tenant_name, socket_path, purge),
        Tenancy::Mariadb
        | Tenancy::Opensearch
        | Tenancy::Rabbitmq
        | Tenancy::BougieServer => Err(eyre!(
            "{} deprovisioner not yet implemented",
            entry.name
        )),
        Tenancy::None => Ok(()),
    }
}
