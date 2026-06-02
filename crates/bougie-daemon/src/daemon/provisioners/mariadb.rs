//! `MariaDB` tenancy: database + user + GRANT per project. SERVICES.md §3.1.
//!
//! Per-project tenant gets:
//!   - a database named `<tenant>`,
//!   - a user `<tenant>@localhost` with a random password,
//!   - `GRANT ALL` on `<tenant>.*`.
//!
//! Auth model: the daemon initialises mariadb with
//! `--auth-root-authentication-method=socket`, so the OS user that
//! owns the data dir (i.e. whoever ran `bougied`) is the root
//! account, and provisioning SQL is executed by the daemon running
//! `mariadb --socket=... -e "..."` without a password. PHP clients
//! always go through the per-tenant user, not root.

use crate::daemon::store_layout;
use crate::daemon::tenants::{self, Tenant};
use bougie_paths::Paths;
use eyre::{eyre, Result, WrapErr};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::Instant;

/// Wait up to this long for mariadbd to start accepting socket
/// connections after `mariadb-install-db` (or after the supervisor
/// spawns it). The supervisor's own health probe already waits for
/// the socket, but the provisioner is run after `start()` returns,
/// so this is mostly a defensive retry against momentary EAGAIN
/// in CI under heavy load.
const PROVISION_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Idempotent first-run bootstrap. Runs `mariadb-install-db` against
/// the per-service data dir when it has no system tables yet. The
/// resulting datadir is what `mariadbd --datadir=...` reads at
/// supervisor start time.
pub async fn pre_start(paths: &Paths) -> Result<()> {
    let entry = crate::daemon::catalog::find("mariadb")
        .ok_or_else(|| eyre!("BUG: mariadb missing from catalog"))?;
    let datadir = paths.service_data("mariadb");
    tokio::fs::create_dir_all(&datadir)
        .await
        .wrap_err_with(|| format!("creating {}", datadir.display()))?;
    // The `mysql/db.MAD` table is created by `mariadb-install-db` and
    // is the cheapest sentinel that the datadir is initialised.
    if tokio::fs::try_exists(datadir.join("mysql/db.MAD"))
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }

    let basedir = store_layout::basedir(paths, entry)
        .wrap_err("resolving mariadb basedir")?;
    let install_db = basedir.join("bin/mariadb-install-db");
    if !tokio::fs::try_exists(&install_db).await.unwrap_or(false) {
        return Err(eyre!(
            "mariadb-install-db not found at {} — is the tarball complete?",
            install_db.display()
        ));
    }

    let user = current_user();
    let mut cmd = Command::new(&install_db);
    cmd
        // CI runners (and some dev hosts) ship a system /etc/my.cnf
        // intended for the OS-vendored MySQL/mariadb. It can set
        // `user=mysql`, inject `mysqlx-*` options our bundled mariadbd
        // doesn't know, etc. `--no-defaults` makes mariadb-install-db
        // ignore every option file; the only inputs it considers are
        // the ones we pass explicitly below.
        .arg("--no-defaults")
        .arg(format!("--basedir={}", basedir.display()))
        .arg(format!("--datadir={}", datadir.display()))
        .arg(format!("--user={user}"))
        // No anonymous test DB; we control the tenant model.
        .arg("--skip-test-db")
        // The OS user that owns the data dir (us) is the root account,
        // no password required for socket-local connections.
        .arg("--auth-root-authentication-method=socket")
        // `--skip-name-resolve` avoids a slow DNS lookup against the
        // bootstrap host record. Tenant grants use `'<t>'@'localhost'`
        // which is matched literally, not resolved.
        .arg("--skip-name-resolve");

    let out = cmd
        .output()
        .await
        .wrap_err_with(|| format!("running {}", install_db.display()))?;
    if !out.status.success() {
        return Err(eyre!(
            "mariadb-install-db failed (exit {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

/// Provision a tenant. Idempotent — repeated calls for the same project
/// re-use the same database and user. The password is **derived** from
/// the project (see [`credentials::derive_password`]), so it's stable
/// across a `down`/`--purge`/re-provision cycle even after the tenant
/// ledger or datadir is wiped — keeping a Magento `app/etc/env.php` that
/// captured it at install time valid.
pub async fn provision(
    paths: &Paths,
    tenants_path: &Path,
    tenant_name: &str,
    project: &Path,
    socket: &Path,
) -> Result<Tenant> {
    let existing = tenants::load_all(tenants_path).await?;
    if let Some(existing_t) = existing.iter().find(|t| t.project == project) {
        return Ok(existing_t.clone());
    }

    // Tenant name must be a safe SQL identifier — the catalog defaults
    // it from `composer.json` `name` which can contain slashes ("acme/blog"),
    // those are pre-sanitised to `acme_blog` by the CLI's `tenant_from_*`
    // helper. Defence in depth: reject anything outside the allowed set
    // here too rather than trust the wire.
    if !is_safe_identifier(tenant_name) {
        return Err(eyre!(
            "mariadb: tenant name `{tenant_name}` contains characters outside [A-Za-z0-9_]; \
             rename via `bougie services add mariadb --tenant=...`"
        ));
    }

    // Derived (not random) so re-provisioning yields the same password
    // and a previously-installed env.php keeps connecting. `ALTER USER`
    // below re-asserts it on the live server, healing any drift from an
    // earlier random-password install.
    let password = crate::daemon::credentials::derive_password(paths, "mariadb", project)?;
    let mariadb_bin = mariadb_client_binary(paths)?;

    wait_for_socket(socket, PROVISION_CONNECT_TIMEOUT)
        .await
        .wrap_err("mariadb socket never became connectable")?;

    let name = tenant_name;
    let sql = format!(
        "CREATE DATABASE IF NOT EXISTS `{name}` \
           CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci; \
         CREATE USER IF NOT EXISTS '{name}'@'localhost' \
           IDENTIFIED BY '{password}'; \
         ALTER USER '{name}'@'localhost' IDENTIFIED BY '{password}'; \
         GRANT ALL PRIVILEGES ON `{name}`.* TO '{name}'@'localhost'; \
         FLUSH PRIVILEGES;",
    );
    run_sql(&mariadb_bin, socket, &sql)
        .await
        .wrap_err_with(|| format!("provisioning mariadb tenant `{tenant_name}`"))?;

    let mut tenant = Tenant::new(tenant_name, project.to_path_buf());
    tenant.secrets.insert("password".into(), password);
    tenants::append(tenants_path, &tenant).await?;
    Ok(tenant)
}

/// Release a tenant. With `purge`, also `DROP DATABASE` + `DROP USER`.
/// Without `purge`, the data survives a `services down` so a later
/// `services up` reuses it (matches redis's behaviour).
pub async fn deprovision(
    paths: &Paths,
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
                "mariadb: refusing to purge tenant with unsafe identifier `{tenant_name}`"
            ));
        }
        let mariadb_bin = mariadb_client_binary(paths)?;
        let name = tenant_name;
        let sql = format!(
            "DROP DATABASE IF EXISTS `{name}`; \
             DROP USER IF EXISTS '{name}'@'localhost';",
        );
        run_sql(&mariadb_bin, sock, &sql)
            .await
            .wrap_err_with(|| format!("purging mariadb tenant `{tenant_name}`"))?;
    }
    tenants::rewrite(tenants_path, |t| t.tenant != tenant_name).await?;
    Ok(())
}

// -------------------- helpers --------------------

fn mariadb_client_binary(paths: &Paths) -> Result<PathBuf> {
    let entry = crate::daemon::catalog::find("mariadb")
        .ok_or_else(|| eyre!("BUG: mariadb missing from catalog"))?;
    let basedir = store_layout::basedir(paths, entry)?;
    let bin = basedir.join("bin/mariadb");
    if !bin.exists() {
        return Err(eyre!(
            "mariadb client not found at {} — is the tarball complete?",
            bin.display()
        ));
    }
    Ok(bin)
}

async fn run_sql(mariadb_bin: &Path, socket: &Path, sql: &str) -> Result<()> {
    // `mariadb-install-db --auth-root-authentication-method=socket`
    // makes mariadbd accept the OS-uid owner as a passwordless root,
    // not the literal user `root`. Connect as the daemon's effective
    // user; mariadbd reads peer credentials from the socket and maps
    // them onto the matching `<user>@localhost` grant created at
    // bootstrap time.
    let os_user = current_user();
    let out = Command::new(mariadb_bin)
        // Same `/etc/my.cnf` poison risk as the install-db / mariadbd
        // invocations: skip every option file and use only the args
        // we hand the client explicitly.
        .arg("--no-defaults")
        .arg(format!("--socket={}", socket.display()))
        .arg(format!("--user={os_user}"))
        .arg("--batch")
        .arg("--skip-column-names")
        .arg("--execute")
        .arg(sql)
        .output()
        .await
        .wrap_err_with(|| format!("invoking {}", mariadb_bin.display()))?;
    if !out.status.success() {
        return Err(eyre!(
            "mariadb client returned non-zero (exit {}): {}",
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
                "mariadb unix socket at {} did not become connectable within {timeout:?}",
                path.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "bougie".into())
}

/// Match `[A-Za-z0-9_]+`. The CLI already substitutes `/` → `_` in
/// composer names; this is defence in depth against a malformed
/// `extra.bougie.services.tenant` override sneaking SQL metacharacters
/// in via the wire.
fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && s.len() <= 64 // mariadb identifier cap
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
