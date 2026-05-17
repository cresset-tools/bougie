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
pub mod rabbitmq;
pub mod redis;

use super::catalog::{CatalogEntry, Tenancy};
use super::tenants::Tenant;
use crate::Paths;
use eyre::{eyre, Result};
use std::path::Path;

/// Helper: run a sync provisioner closure on the blocking pool. Owns
/// the `reqwest::blocking` / `std::process` bridge in one place so
/// callers in the IPC layer can stay pure-async.
async fn run_blocking<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|join_err| eyre!("provisioner task panicked: {join_err}"))?
}

/// One-shot bootstrap hook. Runs after sandbox setup but before the
/// supervisor spawns the service binary. Idempotent — safe to call on
/// every `service.up` invocation. `Ok(())` for services that need no
/// bootstrap.
pub async fn pre_start(entry: &CatalogEntry, paths: &Paths) -> Result<()> {
    match entry.tenancy {
        Tenancy::Mariadb => {
            let paths = paths.clone();
            run_blocking(move || mariadb::pre_start(&paths)).await
        }
        Tenancy::Opensearch => opensearch::pre_start(paths).await,
        Tenancy::Rabbitmq => {
            let paths = paths.clone();
            run_blocking(move || rabbitmq::pre_start(&paths)).await
        }
        Tenancy::BougieServer => {
            let paths = paths.clone();
            run_blocking(move || bougie_server::pre_start(&paths)).await
        }
        Tenancy::Redis | Tenancy::None => Ok(()),
    }
}

/// Dispatch a `provision` call to the right per-service implementation.
/// Returns a `Tenant` ready to be appended to `tenants.json`. For
/// mariadb the append is performed inside the provisioner (it has to
/// stash a generated password); for redis the caller appends.
pub async fn provision(
    entry: &CatalogEntry,
    paths: &Paths,
    tenants_path: &Path,
    tenant_name: &str,
    project: &Path,
) -> Result<Tenant> {
    match entry.tenancy {
        Tenancy::Redis => {
            let tenants_path = tenants_path.to_path_buf();
            let tenant_name = tenant_name.to_string();
            let project = project.to_path_buf();
            run_blocking(move || redis::provision(&tenants_path, &tenant_name, &project)).await
        }
        Tenancy::Mariadb => {
            let paths = paths.clone();
            let tenants_path = tenants_path.to_path_buf();
            let tenant_name = tenant_name.to_string();
            let project = project.to_path_buf();
            run_blocking(move || {
                let socket = paths.service_run("mariadb").join("mariadb.sock");
                mariadb::provision(&paths, &tenants_path, &tenant_name, &project, &socket)
            })
            .await
        }
        Tenancy::Opensearch => {
            opensearch::provision(tenants_path, tenant_name, project).await
        }
        Tenancy::BougieServer => {
            let paths = paths.clone();
            let tenants_path = tenants_path.to_path_buf();
            let tenant_name = tenant_name.to_string();
            let project = project.to_path_buf();
            run_blocking(move || {
                bougie_server::provision(&paths, &tenants_path, &tenant_name, &project)
            })
            .await
        }
        Tenancy::Rabbitmq => {
            let paths = paths.clone();
            let tenants_path = tenants_path.to_path_buf();
            let tenant_name = tenant_name.to_string();
            let project = project.to_path_buf();
            run_blocking(move || {
                rabbitmq::provision(&paths, &tenants_path, &tenant_name, &project)
            })
            .await
        }
        Tenancy::None => Err(eyre!("{} has no user-facing tenancy", entry.name)),
    }
}

/// Inverse of `provision` — symmetric dispatch.
pub async fn deprovision(
    entry: &CatalogEntry,
    paths: &Paths,
    tenants_path: &Path,
    tenant_name: &str,
    socket_path: Option<&Path>,
    purge: bool,
) -> Result<()> {
    match entry.tenancy {
        Tenancy::Redis => {
            let tenants_path = tenants_path.to_path_buf();
            let tenant_name = tenant_name.to_string();
            let socket_path = socket_path.map(Path::to_path_buf);
            run_blocking(move || {
                redis::deprovision(&tenants_path, &tenant_name, socket_path.as_deref(), purge)
            })
            .await
        }
        Tenancy::Mariadb => {
            let paths = paths.clone();
            let tenants_path = tenants_path.to_path_buf();
            let tenant_name = tenant_name.to_string();
            let socket_path = socket_path.map(Path::to_path_buf);
            run_blocking(move || {
                mariadb::deprovision(
                    &paths,
                    &tenants_path,
                    &tenant_name,
                    socket_path.as_deref(),
                    purge,
                )
            })
            .await
        }
        Tenancy::Opensearch => {
            opensearch::deprovision(tenants_path, tenant_name, purge).await
        }
        Tenancy::BougieServer => {
            let paths = paths.clone();
            let tenants_path = tenants_path.to_path_buf();
            let tenant_name = tenant_name.to_string();
            run_blocking(move || {
                bougie_server::deprovision(&paths, &tenants_path, &tenant_name, purge)
            })
            .await
        }
        Tenancy::Rabbitmq => {
            let paths = paths.clone();
            let tenants_path = tenants_path.to_path_buf();
            let tenant_name = tenant_name.to_string();
            run_blocking(move || {
                rabbitmq::deprovision(&paths, &tenants_path, &tenant_name, purge)
            })
            .await
        }
        Tenancy::None => Ok(()),
    }
}
