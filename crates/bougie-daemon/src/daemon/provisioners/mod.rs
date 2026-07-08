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
pub mod mailpit;
pub mod mariadb;
pub mod opensearch;
pub mod rabbitmq;
pub mod redis;

use super::catalog::{CatalogEntry, Tenancy};
use super::tenants::Tenant;
use bougie_paths::Paths;
use eyre::{eyre, Result};
use std::path::Path;

/// One-shot bootstrap hook. Runs after sandbox setup but before the
/// supervisor spawns the service binary. Idempotent — safe to call on
/// every `service.up` invocation. `Ok(())` for services that need no
/// bootstrap.
pub async fn pre_start(entry: &CatalogEntry, paths: &Paths) -> Result<()> {
    match entry.tenancy {
        Tenancy::Mariadb => mariadb::pre_start(paths).await,
        Tenancy::Opensearch => opensearch::pre_start(paths).await,
        Tenancy::Rabbitmq => rabbitmq::pre_start(paths).await,
        Tenancy::BougieServer => bougie_server::pre_start(paths).await,
        // Mailpit needs no bootstrap — the sandbox creates its data dir
        // before spawn and Mailpit creates `mailpit.db` there itself.
        Tenancy::Redis | Tenancy::Mailpit | Tenancy::None => Ok(()),
    }
}

/// Dispatch a `provision` call to the right per-service implementation.
/// Returns a `Tenant` ready to be appended to `tenants.json`. For
/// mariadb the append is performed inside the provisioner (it has to
/// stash a generated password); for redis the caller appends.
#[tracing::instrument(skip_all, fields(service = entry.name, tenant = tenant_name))]
pub async fn provision(
    entry: &CatalogEntry,
    paths: &Paths,
    version: &str,
    tenants_path: &Path,
    tenant_name: &str,
    project: &Path,
) -> Result<Tenant> {
    match entry.tenancy {
        Tenancy::Redis => redis::provision(tenants_path, tenant_name, project).await,
        Tenancy::Mariadb => {
            let socket = paths.service_run("mariadb", version).join("mariadb.sock");
            mariadb::provision(paths, tenants_path, tenant_name, project, &socket).await
        }
        Tenancy::Opensearch => {
            let port =
                crate::daemon::endpoint::effective_primary(paths, "opensearch", version, 9200);
            opensearch::provision(port, tenants_path, tenant_name, project).await
        }
        Tenancy::BougieServer => {
            bougie_server::provision(paths, tenants_path, tenant_name, project).await
        }
        Tenancy::Rabbitmq => {
            rabbitmq::provision(paths, tenants_path, tenant_name, project).await
        }
        Tenancy::Mailpit => mailpit::provision(tenants_path, tenant_name, project).await,
        Tenancy::None => Err(eyre!("{} has no user-facing tenancy", entry.name)),
    }
}

/// Inverse of `provision` — symmetric dispatch.
pub async fn deprovision(
    entry: &CatalogEntry,
    paths: &Paths,
    version: &str,
    tenants_path: &Path,
    tenant_name: &str,
    socket_path: Option<&Path>,
    purge: bool,
) -> Result<()> {
    match entry.tenancy {
        Tenancy::Redis => redis::deprovision(tenants_path, tenant_name, socket_path, purge).await,
        Tenancy::Mariadb => {
            mariadb::deprovision(paths, tenants_path, tenant_name, socket_path, purge).await
        }
        Tenancy::Opensearch => {
            let port =
                crate::daemon::endpoint::effective_primary(paths, "opensearch", version, 9200);
            opensearch::deprovision(port, tenants_path, tenant_name, purge).await
        }
        Tenancy::BougieServer => {
            bougie_server::deprovision(paths, tenants_path, tenant_name, purge).await
        }
        Tenancy::Rabbitmq => {
            rabbitmq::deprovision(paths, tenants_path, tenant_name, purge).await
        }
        Tenancy::Mailpit => mailpit::deprovision(tenants_path, tenant_name, purge).await,
        Tenancy::None => Ok(()),
    }
}
