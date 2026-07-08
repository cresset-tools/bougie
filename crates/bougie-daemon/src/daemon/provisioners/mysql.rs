//! `MySQL` tenancy: database + user + GRANT per project. SERVICES.md §3.1.
//!
//! The tenant model is identical to [`super::mariadb`] — one database,
//! one `<tenant>@localhost` user with a derived password, `GRANT ALL` on
//! that database — but the *bootstrap* and *root auth* differ, which is
//! why this is a separate module rather than a shared one:
//!
//!   - **Bootstrap.** `MySQL` ships no `mariadb-install-db`. The datadir
//!     is initialised with `mysqld --initialize-insecure`, which lays down
//!     the system tables and creates a **passwordless** `root@localhost`.
//!   - **Root auth.** `MariaDB` maps the socket-peer OS uid onto root
//!     (`--auth-root-authentication-method=socket`); `MySQL` has no such
//!     option, so provisioning SQL connects as the literal `root` user
//!     with an empty password (what the insecure init leaves behind).
//!   - **Tenant auth plugin.** Users are created with a plain
//!     `IDENTIFIED BY`, i.e. the server default `caching_sha2_password`.
//!     That works unchanged on both 8.0 and 8.4; forcing
//!     `mysql_native_password` would break 8.4, which disables it by
//!     default.
//!
//! Everything is keyed by the instance `version` so two `MySQL` versions
//! (8.0 beside 8.4) provision into — and are reached through — their own
//! version-keyed datadir + socket.

use crate::daemon::store_layout;
use crate::daemon::tenants::{self, Tenant};
use bougie_paths::Paths;
use eyre::{eyre, Result, WrapErr};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::Instant;

/// Wait up to this long for mysqld to start accepting socket connections
/// during provisioning. Mirrors the mariadb provisioner: the supervisor's
/// health probe already gates on readiness, so this is a defensive retry
/// against momentary EAGAIN under CI load.
const PROVISION_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Idempotent first-run bootstrap. Runs `mysqld --initialize-insecure`
/// against the per-instance datadir when it has no system tables yet.
/// The resulting datadir is what `mysqld --datadir=…` reads at supervisor
/// start time.
pub async fn pre_start(paths: &Paths, version: &str) -> Result<()> {
    let entry = crate::daemon::catalog::find("mysql")
        .ok_or_else(|| eyre!("BUG: mysql missing from catalog"))?;
    let datadir = paths.service_data("mysql", version);
    tokio::fs::create_dir_all(&datadir)
        .await
        .wrap_err_with(|| format!("creating {}", datadir.display()))?;
    // `mysql.ibd` (the InnoDB data-dictionary tablespace) is created by
    // `--initialize` and is the cheapest sentinel that the datadir is
    // already initialised. Present → nothing to do.
    if tokio::fs::try_exists(datadir.join("mysql.ibd"))
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }

    let basedir = store_layout::basedir(paths, entry, version)
        .wrap_err("resolving mysql basedir")?;
    let mysqld = basedir.join("bin/mysqld");
    if !tokio::fs::try_exists(&mysqld).await.unwrap_or(false) {
        return Err(eyre!(
            "mysqld not found at {} — is the tarball complete?",
            mysqld.display()
        ));
    }

    // `--initialize-insecure` requires the datadir to be empty. It is on
    // the happy path (we just created it, and the tmp/ dir + sandbox
    // carve-outs are laid down later, at spawn time). A leftover from a
    // half-finished init would trip "data directory is not empty" — a
    // clear enough error to surface as-is.
    let mut cmd = Command::new(&mysqld);
    cmd
        // Anchor cwd to the datadir; guards against inheriting a
        // since-deleted cwd from the daemon (see the mariadb note).
        .current_dir(&datadir)
        // Ignore the host's /etc/my.cnf — on CI runners it often carries
        // options our bundled mysqld rejects. `--no-defaults` must be the
        // first argument on the mysqld command line.
        .arg("--no-defaults")
        .arg("--initialize-insecure")
        .arg(format!("--basedir={}", basedir.display()))
        .arg(format!("--datadir={}", datadir.display()));
    // Deliberately no `--user`: bougied runs as the unprivileged dev user,
    // and mysqld only honours `--user` when started as root (it warns and
    // ignores it otherwise).

    let out = cmd
        .output()
        .await
        .wrap_err_with(|| format!("running {} --initialize-insecure", mysqld.display()))?;
    if !out.status.success() {
        return Err(eyre!(
            "mysqld --initialize-insecure failed (exit {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

/// Provision a tenant. Idempotent — repeated calls for the same project
/// re-use the database and user. The password is **derived** from the
/// project (see [`credentials::derive_password`]), so it's stable across
/// a `down`/`--purge`/re-provision cycle even after the ledger or datadir
/// is wiped — keeping a captured `app/etc/env.php` valid.
///
/// [`credentials::derive_password`]: crate::daemon::credentials::derive_password
pub async fn provision(
    paths: &Paths,
    version: &str,
    tenants_path: &Path,
    tenant_name: &str,
    project: &Path,
    socket: &Path,
) -> Result<Tenant> {
    let existing = tenants::load_all(tenants_path).await?;
    if let Some(existing_t) = existing.iter().find(|t| t.project == project) {
        return Ok(existing_t.clone());
    }

    // Defence in depth: the CLI pre-sanitises tenant names to
    // `[A-Za-z0-9_]`, but reject anything else off the wire before it
    // reaches an interpolated SQL statement.
    if !is_safe_identifier(tenant_name) {
        return Err(eyre!(
            "mysql: tenant name `{tenant_name}` contains characters outside [A-Za-z0-9_]; \
             rename via `bougie service add mysql --tenant=...`"
        ));
    }

    let password = crate::daemon::credentials::derive_password(paths, "mysql", project)?;
    let mysql_bin = mysql_client_binary(paths, version)?;

    wait_for_socket(socket, PROVISION_CONNECT_TIMEOUT)
        .await
        .wrap_err("mysql socket never became connectable")?;

    let name = tenant_name;
    // Default auth plugin (caching_sha2_password) — no `IDENTIFIED WITH`,
    // so this is identical on 8.0 and 8.4. `ALTER USER` re-asserts the
    // derived password, healing any drift from an earlier install.
    let sql = format!(
        "CREATE DATABASE IF NOT EXISTS `{name}` \
           CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci; \
         CREATE USER IF NOT EXISTS '{name}'@'localhost' \
           IDENTIFIED BY '{password}'; \
         ALTER USER '{name}'@'localhost' IDENTIFIED BY '{password}'; \
         GRANT ALL PRIVILEGES ON `{name}`.* TO '{name}'@'localhost'; \
         FLUSH PRIVILEGES;",
    );
    run_sql(&mysql_bin, socket, &sql)
        .await
        .wrap_err_with(|| format!("provisioning mysql tenant `{tenant_name}`"))?;

    let mut tenant = Tenant::new(tenant_name, project.to_path_buf());
    tenant.secrets.insert("password".into(), password);
    tenants::append(tenants_path, &tenant).await?;
    Ok(tenant)
}

/// Release a tenant. With `purge`, also `DROP DATABASE` + `DROP USER`.
/// Without `purge`, the data survives a `service down` so a later
/// `service up` reuses it (matches mariadb/redis behaviour).
pub async fn deprovision(
    paths: &Paths,
    version: &str,
    tenants_path: &Path,
    tenant_name: &str,
    socket: Option<&Path>,
    purge: bool,
) -> Result<()> {
    let existing = tenants::load_all(tenants_path).await?;
    let Some(_target) = existing.iter().find(|t| t.tenant == tenant_name).cloned() else {
        return Ok(());
    };
    if let (true, Some(sock)) = (purge, socket) {
        if !is_safe_identifier(tenant_name) {
            return Err(eyre!(
                "mysql: refusing to purge tenant with unsafe identifier `{tenant_name}`"
            ));
        }
        let mysql_bin = mysql_client_binary(paths, version)?;
        let name = tenant_name;
        let sql = format!(
            "DROP DATABASE IF EXISTS `{name}`; \
             DROP USER IF EXISTS '{name}'@'localhost';",
        );
        run_sql(&mysql_bin, sock, &sql)
            .await
            .wrap_err_with(|| format!("purging mysql tenant `{tenant_name}`"))?;
    }
    tenants::rewrite(tenants_path, |t| t.tenant != tenant_name).await?;
    Ok(())
}

// -------------------- helpers --------------------

/// Health probe: `SELECT 1` via the bundled `mysql` client. A real
/// readiness check — a bare socket-connect can succeed while mysqld is
/// still in crash recovery and rejecting queries.
pub(crate) async fn health(paths: &Paths, version: &str, socket: &Path) -> Result<()> {
    let bin = mysql_client_binary(paths, version)?;
    run_sql(&bin, socket, "SELECT 1").await
}

fn mysql_client_binary(paths: &Paths, version: &str) -> Result<PathBuf> {
    let entry = crate::daemon::catalog::find("mysql")
        .ok_or_else(|| eyre!("BUG: mysql missing from catalog"))?;
    let basedir = store_layout::basedir(paths, entry, version)?;
    let bin = basedir.join("bin/mysql");
    if !bin.exists() {
        return Err(eyre!(
            "mysql client not found at {} — is the tarball complete?",
            bin.display()
        ));
    }
    Ok(bin)
}

async fn run_sql(mysql_bin: &Path, socket: &Path, sql: &str) -> Result<()> {
    // `--initialize-insecure` leaves `root@localhost` with an empty
    // password, so provisioning connects as literal `root` over the
    // socket. (MariaDB maps the OS uid onto root; MySQL does not.)
    let cwd = socket.parent().unwrap_or_else(|| Path::new("/"));
    let out = Command::new(mysql_bin)
        // The continuous health probe wraps this in a timeout; kill the
        // client if that timeout drops the future so a wedged mysqld
        // can't leave a hung client behind.
        .kill_on_drop(true)
        .current_dir(cwd)
        // Same /etc/my.cnf poison risk as the server invocation.
        .arg("--no-defaults")
        .arg(format!("--socket={}", socket.display()))
        .arg("--user=root")
        .arg("--batch")
        .arg("--skip-column-names")
        .arg("--execute")
        .arg(sql)
        .output()
        .await
        .wrap_err_with(|| format!("invoking {}", mysql_bin.display()))?;
    if !out.status.success() {
        return Err(eyre!(
            "mysql client returned non-zero (exit {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

async fn wait_for_socket(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(eyre!(
                "mysql unix socket at {} did not become connectable within {timeout:?}",
                path.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Match `[A-Za-z0-9_]+` within `MySQL`'s 64-char identifier cap.
fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && s.len() <= 64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_identifier_accepts_typical_tenants() {
        assert!(is_safe_identifier("acme_blog"));
        assert!(is_safe_identifier("acmeBlog"));
        assert!(is_safe_identifier("blog_2026"));
    }

    #[test]
    fn safe_identifier_rejects_sql_metacharacters() {
        assert!(!is_safe_identifier(""));
        assert!(!is_safe_identifier("foo; DROP TABLE x"));
        assert!(!is_safe_identifier("foo'bar"));
        assert!(!is_safe_identifier("foo`bar"));
        assert!(!is_safe_identifier("foo bar"));
        assert!(!is_safe_identifier("foo-bar"));
    }
}
