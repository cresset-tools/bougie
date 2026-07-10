//! Per-tenant `BOUGIE_SERVICE_*` connection variables (SERVICES.md §3.4).
//!
//! [`tenant_service_env`] is the single source of truth for the
//! service-connection vocabulary. Two consumers share it so they can
//! never drift:
//!
//! - the daemon's `service.env` IPC method (what `bougie run` and the
//!   recipe env provider inject into child processes), and
//! - `bougie service credentials`, the offline CLI view of the same
//!   variables.
//!
//! Values come from the catalog (bindings, ports) and the tenant's
//! ledger row (name, alloc, secrets). The function is pure and
//! side-effect free: secrets are read from the row as-is — callers that
//! want the derived-password fallback (see `credentials::derive_password`)
//! resolve it into `tenant.secrets` first.

use crate::daemon::catalog::{self, Binding, CatalogEntry};
use crate::daemon::tenants::Tenant;
use bougie_paths::Paths;
use serde_json::Value;
use std::fmt::Write as _;

/// Every Tcp binding in the catalog is loopback-only in v1
/// (`Binding::Tcp`'s doc comment). Centralising the host here keeps
/// the URL/HOST/PORT vars in sync — recipes can splice them
/// independently without knowing the assembly recipe.
const LOOPBACK: &str = "127.0.0.1";

/// Build the `BOUGIE_SERVICE_<NAME>_*` map for one service's tenant.
///
/// Alloc values keep their ledger JSON type (redis `db_number` stays a
/// number) so the IPC reply is byte-stable; env-file consumers
/// stringify at the edge.
///
/// `version` is the resolved instance version this tenant belongs to —
/// which version-keyed state dir holds its socket / endpoint. For a
/// single-version service it's the catalog default; for a multi-version
/// one (`mysql` 8.0 beside 8.4) it's the concrete version the offline
/// resolver ([`crate::daemon::tenants::instance_versions`]) found the
/// project's ledger row under, so an 8.0 project gets its own 8.0 socket
/// rather than the default 8.4 one.
#[must_use]
#[allow(clippy::too_many_lines, reason = "one arm per catalog service")]
pub fn tenant_service_env(
    paths: &Paths,
    entry: &CatalogEntry,
    version: &str,
    tenant: &Tenant,
) -> serde_json::Map<String, Value> {
    let mut vars = serde_json::Map::new();
    let prefix = format!("BOUGIE_SERVICE_{}_", entry.name.to_ascii_uppercase());

    // For Tcp-bound services, expose HOST/PORT alongside any
    // service-specific URL string. Recipes that need split
    // host/port (Magento's `setup:install --opensearch-host
    // --opensearch-port`, etc.) read these directly instead of
    // parsing URL bytes in shell.
    // Effective primary TCP port — relocated when the catalog default was
    // taken, read from the instance's endpoint.json; falls back to the
    // catalog default when unrecorded (works offline, no daemon needed).
    let tcp_port = match entry.binding {
        Binding::Tcp { port } => {
            Some(crate::daemon::endpoint::effective_primary(paths, entry.name, version, port))
        }
        Binding::UnixSocket { .. } | Binding::None => None,
    };
    if let Some(port) = tcp_port {
        vars.insert(format!("{prefix}HOST"), Value::String(LOOPBACK.into()));
        vars.insert(format!("{prefix}PORT"), Value::String(port.to_string()));
    }

    match entry.name {
        "redis" => {
            // Emit the STABLE per-project socket (a symlink the daemon
            // repoints at the live instance), not the version-keyed
            // instance socket — so a path baked into app config survives
            // version bumps + the instance-socket relocation.
            let sock = paths
                .project_conn_socket(&tenant.project, "redis.sock")
                .display()
                .to_string();
            vars.insert(format!("{prefix}SOCKET"), Value::String(sock));
            if let Some(db) = tenant.alloc.get("db_number") {
                vars.insert(format!("{prefix}DB"), db.clone());
            }
        }
        // mariadb and mysql share the database+user+password vocabulary;
        // they differ only in the socket file name (and, upstream, the
        // provisioner). Fold them into one arm keyed off the catalog
        // socket name so the two never drift.
        "mariadb" | "mysql" => {
            let sockname = match entry.binding {
                Binding::UnixSocket { sockname } => sockname,
                _ => "mysql.sock",
            };
            // Stable per-project socket (symlink → live instance), not the
            // version-keyed instance socket: this is what `--db-host`
            // bakes into env.php at install time, so it must not move when
            // the DB version bumps.
            let sock = paths
                .project_conn_socket(&tenant.project, sockname)
                .display()
                .to_string();
            vars.insert(format!("{prefix}SOCKET"), Value::String(sock));
            vars.insert(
                format!("{prefix}DATABASE"),
                Value::String(tenant.tenant.clone()),
            );
            vars.insert(
                format!("{prefix}USER"),
                Value::String(tenant.tenant.clone()),
            );
            if let Some(pw) = tenant.secrets.get("password") {
                vars.insert(format!("{prefix}PASSWORD"), Value::String(pw.clone()));
            }
        }
        "opensearch" => {
            // URL composed from the catalog port (set above as
            // _HOST/_PORT). Surface the tenant's reserved index
            // prefix so apps build `<prefix>articles` etc.
            if let Some(port) = tcp_port {
                vars.insert(
                    format!("{prefix}URL"),
                    Value::String(format!("http://{LOOPBACK}:{port}")),
                );
            }
            if let Some(p) = tenant.alloc.get("index_prefix") {
                vars.insert(format!("{prefix}INDEX_PREFIX"), p.clone());
            }
        }
        "server" => {
            // Root URL alongside the tenant's reserved hostname
            // so apps can build absolute redirects without
            // re-encoding the suffix.
            if let Some(port) = tcp_port {
                vars.insert(
                    format!("{prefix}URL"),
                    Value::String(format!("http://{LOOPBACK}:{port}")),
                );
            }
            if let Some(h) = tenant.alloc.get("hostname") {
                vars.insert(format!("{prefix}HOSTNAME"), h.clone());
            }
        }
        "rabbitmq" => {
            // Compose the full AMQP DSN so apps don't have to
            // assemble the pieces; vhost lives in the path
            // component, user and password in the authority.
            let user = tenant
                .alloc
                .get("username")
                .and_then(|v| v.as_str())
                .unwrap_or(&tenant.tenant);
            let vhost = tenant
                .alloc
                .get("vhost")
                .and_then(|v| v.as_str())
                .unwrap_or(&tenant.tenant);
            let pw = tenant.secrets.get("password").cloned().unwrap_or_default();
            if let Some(port) = tcp_port {
                let url = format!(
                    "amqp://{}:{}@{LOOPBACK}:{port}/{}",
                    urlencode(user),
                    urlencode(&pw),
                    urlencode(vhost),
                );
                vars.insert(format!("{prefix}URL"), Value::String(url));
            }
            vars.insert(format!("{prefix}VHOST"), Value::String(vhost.to_string()));
            vars.insert(format!("{prefix}USER"), Value::String(user.to_string()));
            if !pw.is_empty() {
                vars.insert(format!("{prefix}PASSWORD"), Value::String(pw));
            }
        }
        "mailpit" => {
            // SMTP host/port are already emitted as _HOST/_PORT
            // from the Tcp binding above. Compose the Symfony-Mailer
            // style DSN from the same port so apps can splice
            // `MAILER_DSN` directly (no auth — the dev sink accepts
            // any/no credentials).
            if let Some(port) = tcp_port {
                vars.insert(
                    format!("{prefix}DSN"),
                    Value::String(format!("smtp://{LOOPBACK}:{port}")),
                );
            }
            // The human-facing web UI / REST API lives on a second
            // port the single-endpoint binding can't model; surface
            // it explicitly so `bougie run` users can open it.
            vars.insert(
                format!("{prefix}DASHBOARD_URL"),
                Value::String(format!("http://{LOOPBACK}:{}", crate::daemon::endpoint::effective_extra(paths, "mailpit", version, "http", catalog::MAILPIT_HTTP_PORT))),
            );
        }
        _ => {}
    }
    vars
}

/// Percent-encode the AMQP-DSN-significant characters. Tenant names
/// and passwords today are constrained to `[a-z0-9_]+` and hex
/// respectively, so the encoder is a no-op on the happy path; it's
/// defence-in-depth against a future widening of those validators.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => write!(out, "%{b:02X}").expect("writing to String"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn paths_in(dir: &TempDir) -> Paths {
        Paths::new(dir.path().to_path_buf(), dir.path().join("cache"))
    }

    fn tenant(name: &str) -> Tenant {
        Tenant::new(name, "/p/acme")
    }

    #[test]
    fn mariadb_emits_socket_database_user_password() {
        let dir = TempDir::new().unwrap();
        let paths = paths_in(&dir);
        let entry = catalog::find("mariadb").unwrap();
        let mut t = tenant("acme");
        t.secrets.insert("password".into(), "deadbeef".into());

        let vars = tenant_service_env(&paths, entry, entry.version, &t);
        assert_eq!(
            vars["BOUGIE_SERVICE_MARIADB_DATABASE"],
            Value::String("acme".into())
        );
        assert_eq!(
            vars["BOUGIE_SERVICE_MARIADB_USER"],
            Value::String("acme".into())
        );
        assert_eq!(
            vars["BOUGIE_SERVICE_MARIADB_PASSWORD"],
            Value::String("deadbeef".into())
        );
        let sock = vars["BOUGIE_SERVICE_MARIADB_SOCKET"].as_str().unwrap();
        assert!(sock.ends_with("mariadb.sock"), "{sock}");
        // Unix-socket service: no HOST/PORT pair.
        assert!(!vars.contains_key("BOUGIE_SERVICE_MARIADB_HOST"));
    }

    #[test]
    fn mariadb_without_ledger_password_omits_the_var() {
        let dir = TempDir::new().unwrap();
        let paths = paths_in(&dir);
        let entry = catalog::find("mariadb").unwrap();
        let vars = tenant_service_env(&paths, entry, entry.version, &tenant("acme"));
        assert!(!vars.contains_key("BOUGIE_SERVICE_MARIADB_PASSWORD"));
    }

    #[test]
    fn mysql_socket_is_the_stable_per_project_conn_path() {
        // The emitted SOCKET is the project's STABLE connection socket (a
        // symlink the daemon repoints at the live instance), not the
        // version-keyed instance socket — so a path baked into env.php at
        // install time survives DB version bumps. It therefore must NOT
        // depend on the version arg; the version-keying lives in the
        // symlink target (proven in the daemon conn-link test).
        let dir = TempDir::new().unwrap();
        let paths = paths_in(&dir);
        let entry = catalog::find("mysql").unwrap();
        let mut t = tenant("acme");
        t.secrets.insert("password".into(), "deadbeef".into());

        let sock_80 = tenant_service_env(&paths, entry, "8.0.46", &t)
            ["BOUGIE_SERVICE_MYSQL_SOCKET"]
            .as_str()
            .unwrap()
            .to_string();
        let sock_84 = tenant_service_env(&paths, entry, "8.4.10", &t)
            ["BOUGIE_SERVICE_MYSQL_SOCKET"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(sock_80, sock_84, "socket must not depend on the DB version");
        assert_eq!(
            sock_80,
            paths.project_conn_socket(&t.project, "mysql.sock").display().to_string()
        );
        assert!(sock_80.ends_with("mysql.sock"), "{sock_80}");

        let vars = tenant_service_env(&paths, entry, "8.0.46", &t);
        assert_eq!(
            vars["BOUGIE_SERVICE_MYSQL_DATABASE"],
            Value::String("acme".into())
        );
        assert_eq!(
            vars["BOUGIE_SERVICE_MYSQL_PASSWORD"],
            Value::String("deadbeef".into())
        );
        // Unix-socket service — no HOST/PORT.
        assert!(!vars.contains_key("BOUGIE_SERVICE_MYSQL_HOST"));
    }

    #[test]
    fn redis_db_number_keeps_its_json_type() {
        let dir = TempDir::new().unwrap();
        let paths = paths_in(&dir);
        let entry = catalog::find("redis").unwrap();
        let mut t = tenant("acme");
        t.alloc.insert("db_number".into(), Value::from(3));

        let vars = tenant_service_env(&paths, entry, entry.version, &t);
        assert_eq!(vars["BOUGIE_SERVICE_REDIS_DB"], Value::from(3));
        assert!(
            vars["BOUGIE_SERVICE_REDIS_SOCKET"]
                .as_str()
                .unwrap()
                .ends_with("redis.sock")
        );
    }

    #[test]
    fn rabbitmq_composes_percent_encoded_amqp_url() {
        let dir = TempDir::new().unwrap();
        let paths = paths_in(&dir);
        let entry = catalog::find("rabbitmq").unwrap();
        let mut t = tenant("acme");
        t.alloc.insert("vhost".into(), Value::String("acme".into()));
        t.alloc
            .insert("username".into(), Value::String("acme".into()));
        t.secrets.insert("password".into(), "p@ss/word".into());

        let vars = tenant_service_env(&paths, entry, entry.version, &t);
        assert_eq!(
            vars["BOUGIE_SERVICE_RABBITMQ_URL"],
            Value::String("amqp://acme:p%40ss%2Fword@127.0.0.1:5672/acme".into())
        );
        assert_eq!(
            vars["BOUGIE_SERVICE_RABBITMQ_PASSWORD"],
            Value::String("p@ss/word".into())
        );
        assert_eq!(
            vars["BOUGIE_SERVICE_RABBITMQ_PORT"],
            Value::String("5672".into())
        );
    }

    #[test]
    fn mailpit_emits_dsn_and_dashboard_url() {
        let dir = TempDir::new().unwrap();
        let paths = paths_in(&dir);
        let entry = catalog::find("mailpit").unwrap();
        let vars = tenant_service_env(&paths, entry, entry.version, &tenant("acme"));
        assert_eq!(
            vars["BOUGIE_SERVICE_MAILPIT_DSN"],
            Value::String("smtp://127.0.0.1:1025".into())
        );
        assert_eq!(
            vars["BOUGIE_SERVICE_MAILPIT_DASHBOARD_URL"],
            Value::String("http://127.0.0.1:8025".into())
        );
    }

    #[test]
    fn opensearch_and_server_surface_url_plus_alloc() {
        let dir = TempDir::new().unwrap();
        let paths = paths_in(&dir);

        let os = catalog::find("opensearch").unwrap();
        let mut t = tenant("acme");
        t.alloc
            .insert("index_prefix".into(), Value::String("acme_".into()));
        let vars = tenant_service_env(&paths, os, os.version, &t);
        assert_eq!(
            vars["BOUGIE_SERVICE_OPENSEARCH_URL"],
            Value::String("http://127.0.0.1:9200".into())
        );
        assert_eq!(
            vars["BOUGIE_SERVICE_OPENSEARCH_INDEX_PREFIX"],
            Value::String("acme_".into())
        );

        let srv = catalog::find("server").unwrap();
        let mut t = tenant("acme");
        t.alloc
            .insert("hostname".into(), Value::String("acme.bougie.run".into()));
        let vars = tenant_service_env(&paths, srv, srv.version, &t);
        assert_eq!(
            vars["BOUGIE_SERVICE_SERVER_HOSTNAME"],
            Value::String("acme.bougie.run".into())
        );
    }
}
