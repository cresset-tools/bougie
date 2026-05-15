//! Per-service multi-tenancy provisioners.
//!
//! Each entry in the catalog (`Tenancy::*`) maps to one impl module.
//! Three hooks land here:
//!
//! 1. `pre_start` — one-shot bootstrap before the supervisor spawns the
//!    binary. mariadb-install-db lives here; redis has no bootstrap.
//! 2. `provision` — once the service is up, attach a per-project tenant
//!    (DB number / database+user / vhost+user / index template).
//! 3. `deprovision` — symmetric to `provision`; `purge` opt-in destroys
//!    persisted state.
//!
//! Phase 6 wires mariadb. opensearch / rabbitmq / bougie-server land
//! in Phases 7 / 10 / 8 respectively.

pub mod bougie_server;
pub mod mariadb;
pub mod opensearch;
pub mod redis;

use super::catalog::{CatalogEntry, Tenancy};
use super::tenants::Tenant;
use crate::Paths;
use eyre::{eyre, Result};
use std::path::Path;

/// One-shot bootstrap hook. Runs after sandbox setup but before the
/// supervisor spawns the service binary. Idempotent — safe to call on
/// every `service.up` invocation. `Ok(())` for services that need no
/// bootstrap.
pub fn pre_start(entry: &CatalogEntry, paths: &Paths) -> Result<()> {
    match entry.tenancy {
        Tenancy::Mariadb => mariadb::pre_start(paths),
        Tenancy::Opensearch => opensearch::pre_start(paths),
        Tenancy::BougieServer => bougie_server::pre_start(paths),
        Tenancy::Redis | Tenancy::Rabbitmq | Tenancy::None => Ok(()),
    }
}

/// Dispatch a `provision` call to the right per-service implementation.
/// Returns a `Tenant` ready to be appended to `tenants.json`. For
/// mariadb the append is performed inside the provisioner (it has to
/// stash a generated password); for redis the caller appends.
pub fn provision(
    entry: &CatalogEntry,
    paths: &Paths,
    tenants_path: &Path,
    tenant_name: &str,
    project: &Path,
) -> Result<Tenant> {
    match entry.tenancy {
        Tenancy::Redis => redis::provision(tenants_path, tenant_name, project),
        Tenancy::Mariadb => {
            let socket = paths.service_run("mariadb").join("mariadb.sock");
            mariadb::provision(paths, tenants_path, tenant_name, project, &socket)
        }
        Tenancy::Opensearch => opensearch::provision(tenants_path, tenant_name, project),
        Tenancy::BougieServer => {
            bougie_server::provision(paths, tenants_path, tenant_name, project)
        }
        Tenancy::Rabbitmq => Err(eyre!(
            "{} provisioner not yet implemented",
            entry.name
        )),
        Tenancy::None => Err(eyre!("{} has no user-facing tenancy", entry.name)),
    }
}

/// Inverse of `provision` — symmetric dispatch.
pub fn deprovision(
    entry: &CatalogEntry,
    paths: &Paths,
    tenants_path: &Path,
    tenant_name: &str,
    socket_path: Option<&Path>,
    purge: bool,
) -> Result<()> {
    match entry.tenancy {
        Tenancy::Redis => redis::deprovision(tenants_path, tenant_name, socket_path, purge),
        Tenancy::Mariadb => {
            mariadb::deprovision(paths, tenants_path, tenant_name, socket_path, purge)
        }
        Tenancy::Opensearch => opensearch::deprovision(tenants_path, tenant_name, purge),
        Tenancy::BougieServer => {
            bougie_server::deprovision(paths, tenants_path, tenant_name, purge)
        }
        Tenancy::Rabbitmq => Err(eyre!(
            "{} deprovisioner not yet implemented",
            entry.name
        )),
        Tenancy::None => Ok(()),
    }
}
