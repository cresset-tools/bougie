//! Mailpit tenancy: a shared, global mail sink.
//!
//! Unlike mariadb/redis/rabbitmq, Mailpit has no per-project isolation
//! to allocate — every project that opts in shares the one running
//! instance: the same SMTP endpoint (`127.0.0.1:1025`) and the same web
//! UI (`127.0.0.1:8025`). Mailpit's `--tenant-id` is a single-value
//! startup flag that scopes the *whole instance's* database, not a
//! per-connection selector, so one shared instance can't isolate
//! tenants the way an AMQP vhost or a logical redis DB can.
//!
//! So "provisioning" here is deliberately thin: record a bare ledger
//! row marking that this project uses Mailpit. That row is what makes
//! `bougie projects list` show the project and what makes
//! `dispatch_env` emit the project's `BOUGIE_SERVICE_MAILPIT_*` vars.
//! There is no allocation, no credential, and nothing to tear down on
//! the live process — deprovision just drops the row.

use crate::daemon::tenants::{self, Tenant};
use eyre::Result;
use std::path::Path;

/// Record this project's use of the shared Mailpit instance. Idempotent
/// — a project that's already in the ledger gets its existing row back,
/// so repeated `bougie service up` calls don't duplicate it.
pub async fn provision(tenants_path: &Path, tenant_name: &str, project: &Path) -> Result<Tenant> {
    let existing = tenants::load_all(tenants_path).await?;
    if let Some(existing_t) = existing.iter().find(|t| t.project == project) {
        return Ok(existing_t.clone());
    }

    // No alloc, no secrets — the endpoint is identical for every
    // tenant. The row exists purely so the project is visible to
    // `projects list` / `service.env`.
    let tenant = Tenant::new(tenant_name, project.to_path_buf());
    tenants::append(tenants_path, &tenant).await?;
    Ok(tenant)
}

/// Drop this project's ledger row. `purge` is intentionally a no-op:
/// the mailbox is shared across every project on the instance, so
/// there's no per-project mail to delete — wiping it would destroy
/// other projects' caught mail. Clear the shared mailbox from the web
/// UI's "Delete all" button (or `DELETE /api/v1/messages`) instead.
pub async fn deprovision(
    tenants_path: &Path,
    tenant_name: &str,
    _purge: bool,
) -> Result<()> {
    tenants::rewrite(tenants_path, |t| t.tenant != tenant_name).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn provision_records_a_bare_tenant() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        let t = provision(&path, "acme_blog", Path::new("/work/blog")).await.unwrap();
        assert_eq!(t.tenant, "acme_blog");
        // Shared sink: no allocation, no secrets.
        assert!(t.alloc.is_empty(), "{:?}", t.alloc);
        assert!(t.secrets.is_empty(), "{:?}", t.secrets);
    }

    #[tokio::test]
    async fn provision_is_idempotent_per_project() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        let first = provision(&path, "a", Path::new("/p/a")).await.unwrap();
        let again = provision(&path, "a", Path::new("/p/a")).await.unwrap();
        assert_eq!(first.tenant, again.tenant);
        // No second line in the ledger.
        assert_eq!(tenants::load_all(&path).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn two_projects_each_get_a_row() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        provision(&path, "a", Path::new("/p/a")).await.unwrap();
        provision(&path, "b", Path::new("/p/b")).await.unwrap();
        assert_eq!(tenants::load_all(&path).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn deprovision_drops_only_the_named_tenant() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        provision(&path, "a", Path::new("/p/a")).await.unwrap();
        provision(&path, "b", Path::new("/p/b")).await.unwrap();
        deprovision(&path, "a", false).await.unwrap();
        let remaining = tenants::load_all(&path).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].tenant, "b");
    }

    #[tokio::test]
    async fn deprovision_purge_still_only_drops_the_row() {
        // Purge can't isolate per-project mail on a shared sink, so it
        // behaves identically to a plain deprovision: drop the ledger
        // row, leave the shared mailbox untouched.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tenants.json");
        provision(&path, "a", Path::new("/p/a")).await.unwrap();
        deprovision(&path, "a", true).await.unwrap();
        assert!(tenants::load_all(&path).await.unwrap().is_empty());
    }
}
