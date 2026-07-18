//! `MariaDB` tenancy: database + user + GRANT per project. SERVICES.md §3.1.
//!
//! Per-project tenant gets:
//!   - a database named `<tenant>`,
//!   - a user `<tenant>@localhost` with a random password,
//!   - `GRANT ALL` on `<tenant>.*` **and** on the `<tenant>_%` scratch
//!     namespace, so the tenant can create throwaway databases
//!     (`<tenant>_t1`, template+copy, parallel test workers) without needing
//!     socket-root admin access (SERVICES.md §3.1, issue #480). Underscores in
//!     the grant patterns are backslash-escaped so the exact-name grant can't
//!     leak onto sibling databases via `_`'s wildcard meaning.
//!
//! Namespace convention: a tenant literally named as another's prefix (e.g.
//! `acme` vs `acme_blog`) would have its `acme_%` scratch grant overlap
//! `acme_blog`'s exact database. Tenant names come from `composer.json` names
//! sanitised to `[A-Za-z0-9_]`, so this only bites deliberately-colliding
//! project names; documented rather than guarded.
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
pub async fn pre_start(paths: &Paths, version: &str) -> Result<()> {
    let entry = crate::daemon::catalog::find("mariadb")
        .ok_or_else(|| eyre!("BUG: mariadb missing from catalog"))?;
    let datadir = paths.service_data("mariadb", version);
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

    let basedir = store_layout::basedir(paths, entry, version)
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
        // Anchor cwd to the data dir (created just above). Belt-and-
        // suspenders against inheriting a since-deleted cwd from the
        // daemon — see the rabbitmq build_ctl_env note for the failure
        // mode this guards against.
        .current_dir(&datadir)
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
             rename via `bougie service add mariadb --tenant=...`"
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

    run_sql(&mariadb_bin, socket, &provision_sql(tenant_name, &password))
        .await
        .wrap_err_with(|| format!("provisioning mariadb tenant `{tenant_name}`"))?;

    let mut tenant = Tenant::new(tenant_name, project.to_path_buf());
    tenant.secrets.insert("password".into(), password);
    tenants::append(tenants_path, &tenant).await?;
    Ok(tenant)
}

/// Release a tenant. With `purge`, also `DROP DATABASE` + `DROP USER`.
/// Without `purge`, the data survives a `service down` so a later
/// `service up` reuses it (matches redis's behaviour).
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
        // Enumerate the tenant's scratch databases (`<tenant>_*`) so purge
        // drops them too — `DROP DATABASE` takes no wildcard (issue #480).
        let listed = run_sql_output(&mariadb_bin, sock, &scratch_list_sql(tenant_name))
            .await
            .wrap_err_with(|| format!("listing scratch databases for `{tenant_name}`"))?;
        let scratch: Vec<String> = listed
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        run_sql(&mariadb_bin, sock, &purge_sql(tenant_name, &scratch))
            .await
            .wrap_err_with(|| format!("purging mariadb tenant `{tenant_name}`"))?;
    }
    tenants::rewrite(tenants_path, |t| t.tenant != tenant_name).await?;
    Ok(())
}

// -------------------- helpers --------------------

/// Health probe: `SELECT 1` via the bundled `mariadb` client. A real
/// readiness check — the bare socket-connect the supervisor used before
/// can succeed while mariadbd is still in crash recovery and rejecting
/// queries; running a trivial query proves it's actually serving.
pub(crate) async fn health(paths: &Paths, socket: &Path) -> Result<()> {
    let bin = mariadb_client_binary(paths)?;
    run_sql(&bin, socket, "SELECT 1").await
}

fn mariadb_client_binary(paths: &Paths) -> Result<PathBuf> {
    let entry = crate::daemon::catalog::find("mariadb")
        .ok_or_else(|| eyre!("BUG: mariadb missing from catalog"))?;
    let basedir = store_layout::basedir(paths, entry, &entry.version)?;
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
    run_sql_output(mariadb_bin, socket, sql).await.map(|_| ())
}

/// Like [`run_sql`], but returns the client's stdout — used to read back the
/// rows of a `SELECT` (e.g. enumerating a tenant's scratch databases).
async fn run_sql_output(mariadb_bin: &Path, socket: &Path, sql: &str) -> Result<String> {
    // `mariadb-install-db --auth-root-authentication-method=socket`
    // makes mariadbd accept the OS-uid owner as a passwordless root,
    // not the literal user `root`. Connect as the daemon's effective
    // user; mariadbd reads peer credentials from the socket and maps
    // them onto the matching `<user>@localhost` grant created at
    // bootstrap time.
    let os_user = current_user();
    // Anchor cwd to the socket's directory (the service run dir, always
    // present while mariadbd is up) so the client never inherits a
    // since-deleted cwd from the daemon — see the rabbitmq
    // build_ctl_env note for the failure mode this guards against.
    let cwd = socket.parent().unwrap_or_else(|| Path::new("/"));
    let out = Command::new(mariadb_bin)
        // The continuous health probe wraps this in a timeout; kill the
        // client if that timeout drops the future so a wedged mariadbd
        // can't leave a hung client process behind. A no-op on the
        // provisioning path, which always awaits to completion.
        .kill_on_drop(true)
        .current_dir(cwd)
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
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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

/// Backslash-escape `_` and `%` so a tenant name is a *literal* in a GRANT
/// database pattern. In `GRANT ... ON db.*`, MariaDB/MySQL treat `_` and `%`
/// as LIKE-style wildcards even inside backticks, so an exact-name grant on
/// `acme_blog` would otherwise also cover `acmeXblog` and friends. Tenant
/// names are `[A-Za-z0-9_]`, so only `_` can actually occur; `%` is escaped
/// too for defence in depth.
fn escape_grant_pattern(name: &str) -> String {
    name.replace('_', "\\_").replace('%', "\\%")
}

/// The provisioning SQL: the tenant database + user, plus grants on the exact
/// database **and** the `<tenant>_%` scratch namespace (issue #480). Callers
/// must pass a name already checked by [`is_safe_identifier`].
fn provision_sql(name: &str, password: &str) -> String {
    let db = escape_grant_pattern(name);
    format!(
        "CREATE DATABASE IF NOT EXISTS `{name}` \
           CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci; \
         CREATE USER IF NOT EXISTS '{name}'@'localhost' \
           IDENTIFIED BY '{password}'; \
         ALTER USER '{name}'@'localhost' IDENTIFIED BY '{password}'; \
         GRANT ALL PRIVILEGES ON `{db}`.* TO '{name}'@'localhost'; \
         GRANT ALL PRIVILEGES ON `{db}\\_%`.* TO '{name}'@'localhost'; \
         FLUSH PRIVILEGES;",
    )
}

/// SQL that lists the tenant's scratch databases (`<tenant>_*`). A
/// `LEFT(SCHEMA_NAME, n) = '<tenant>_'` prefix test avoids `LIKE`, so the
/// literal underscore in the prefix needs no wildcard-escaping. The main
/// database `<tenant>` (no trailing `_`) is deliberately excluded — it's
/// dropped by name in [`purge_sql`].
fn scratch_list_sql(name: &str) -> String {
    let prefix = format!("{name}_");
    format!(
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA \
         WHERE LEFT(SCHEMA_NAME, {}) = '{prefix}';",
        prefix.len(),
    )
}

/// SQL to purge a tenant: drop each scratch database (real names read back via
/// [`scratch_list_sql`]), then the main database and the user.
fn purge_sql(name: &str, scratch_dbs: &[String]) -> String {
    let mut sql = String::new();
    for db in scratch_dbs {
        // Real DB names off the server — backtick-quote and double any
        // backtick so a weird scratch name can't break out of the identifier.
        sql.push_str(&format!("DROP DATABASE IF EXISTS `{}`; ", db.replace('`', "``")));
    }
    sql.push_str(&format!(
        "DROP DATABASE IF EXISTS `{name}`; DROP USER IF EXISTS '{name}'@'localhost';"
    ));
    sql
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

    #[test]
    fn escape_grant_pattern_escapes_wildcards() {
        // Underscores become literal so the grant can't leak onto siblings.
        assert_eq!(escape_grant_pattern("acme_blog"), "acme\\_blog");
        assert_eq!(escape_grant_pattern("plain"), "plain");
        // `%` never occurs in a real tenant name, but is escaped defensively.
        assert_eq!(escape_grant_pattern("a%b_c"), "a\\%b\\_c");
    }

    #[test]
    fn provision_sql_grants_exact_and_scratch_namespace() {
        let sql = provision_sql("acme_blog", "secret");
        // Exact-name grant with the underscore escaped (literal database).
        assert!(
            sql.contains("GRANT ALL PRIVILEGES ON `acme\\_blog`.* TO 'acme_blog'@'localhost'"),
            "exact grant missing/unescaped: {sql}"
        );
        // Scratch-namespace grant: `<tenant>_%`, both underscores escaped, `%`
        // left as the wildcard.
        assert!(
            sql.contains("GRANT ALL PRIVILEGES ON `acme\\_blog\\_%`.* TO 'acme_blog'@'localhost'"),
            "scratch grant missing/malformed: {sql}"
        );
        // Database + user still created; user name is unescaped (not a pattern).
        assert!(sql.contains("CREATE DATABASE IF NOT EXISTS `acme_blog`"));
        assert!(sql.contains("CREATE USER IF NOT EXISTS 'acme_blog'@'localhost' IDENTIFIED BY 'secret'"));
    }

    #[test]
    fn scratch_list_sql_prefix_matches_only_the_namespace() {
        // Prefix is `<tenant>_` (10 chars for acme_blog), matched with LEFT so
        // the underscore stays literal and the main DB `acme_blog` is excluded.
        assert_eq!(
            scratch_list_sql("acme_blog"),
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA \
             WHERE LEFT(SCHEMA_NAME, 10) = 'acme_blog_';",
        );
    }

    #[test]
    fn purge_sql_drops_scratch_then_main_and_user() {
        let sql = purge_sql("acme_blog", &["acme_blog_t1".into(), "acme_blog_t2".into()]);
        assert!(sql.contains("DROP DATABASE IF EXISTS `acme_blog_t1`;"));
        assert!(sql.contains("DROP DATABASE IF EXISTS `acme_blog_t2`;"));
        assert!(sql.contains("DROP DATABASE IF EXISTS `acme_blog`;"));
        assert!(sql.contains("DROP USER IF EXISTS 'acme_blog'@'localhost';"));
        // No scratch DBs → just the main database + user.
        let bare = purge_sql("acme_blog", &[]);
        assert!(!bare.contains("acme_blog_"));
        assert!(bare.contains("DROP DATABASE IF EXISTS `acme_blog`;"));
    }
}
