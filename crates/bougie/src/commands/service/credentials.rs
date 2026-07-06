//! `bougie service credentials [NAME] [--env]` — print this project's
//! tenant connection info, secrets included.
//!
//! The offline counterpart of the daemon's `service.env` IPC method:
//! both render the same vocabulary via
//! `bougie_daemon::daemon::tenant_env::tenant_service_env`, so what
//! this command prints is exactly what `bougie run` injects. The
//! intended audience is external clients that can't go through
//! `bougie run` or the argv[0] shims — GUI database tools, API
//! explorers — which is why secrets are shown in the clear here while
//! `bougie projects list` (the cross-project view) keeps redacting
//! them: this verb only ever reads the current project's tenants.
//!
//! Connection info is assembled offline from the tenant ledger +
//! derived password, the same sources `bougie service exec` and the
//! `PhpStorm` data-source writer read — no daemon round-trip. The
//! service itself has to be running for the values to connect to
//! anything, but printing them never depends on `bougied`.

use super::config_mut::locate_project_root;
use super::exec::{find_tenant, no_tenant_err};
use bougie_cli::OutputFormat;
use bougie_daemon::daemon::catalog::{self, CatalogEntry, Tenancy};
use bougie_daemon::daemon::credentials::derive_password;
use bougie_daemon::daemon::tenant_env::tenant_service_env;
use bougie_daemon::daemon::tenants::Tenant;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use eyre::{Result, eyre};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct CredentialsResult {
    pub schema_version: u32,
    pub project: String,
    /// Catalog order, one entry per provisioned tenant.
    pub services: Vec<ServiceCredentials>,
    /// Declared services with no tenant row yet (never `up`'d).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_provisioned: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ServiceCredentials {
    pub service: String,
    pub tenant: String,
    /// Connection fields under friendly keys (`password`, `socket`,
    /// `url`, …) — the `BOUGIE_SERVICE_<NAME>_` prefix stripped and
    /// lowercased.
    pub connection: BTreeMap<String, Value>,
    /// The same fields under their canonical env-var names, for the
    /// `--env` printer. Not serialized: JSON consumers get
    /// `connection`; env consumers get the `--env` format.
    #[serde(skip)]
    vars: serde_json::Map<String, Value>,
}

impl Render for CredentialsResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.services.is_empty() && self.not_provisioned.is_empty() {
            writeln!(w, "no services declared in {}", self.project)?;
            return Ok(());
        }
        let mut wrote_any = false;
        for s in &self.services {
            if wrote_any {
                writeln!(w)?;
            }
            wrote_any = true;
            writeln!(w, "{} (tenant {})", s.service, s.tenant)?;
            let width = s.connection.keys().map(String::len).max().unwrap_or(0);
            for (k, v) in &s.connection {
                writeln!(w, "  {k:<width$}  {}", value_str(v))?;
            }
        }
        if !self.not_provisioned.is_empty() {
            if wrote_any {
                writeln!(w)?;
            }
            for n in &self.not_provisioned {
                writeln!(w, "{n}: not provisioned — run `bougie up {n}` first")?;
            }
        }
        Ok(())
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "owned strings from clap-parsed CLI"
)]
pub fn run(format: OutputFormat, name: Option<String>, env: bool) -> Result<ExitCode> {
    let project_root = locate_project_root()?;
    let paths = Paths::from_env()?;

    // Explicit name: any user-facing catalog service with a ledger row
    // qualifies, declared or not (credentials outlive `service remove`
    // until a purge). No name: the project's declared set, in catalog
    // order so output is deterministic.
    let entries: Vec<&'static CatalogEntry> = if let Some(n) = &name {
        let entry = catalog::find(n).filter(|e| e.user_facing).ok_or_else(|| {
            eyre!(
                "unknown service `{n}`; known: {}",
                catalog::user_facing_names()
            )
        })?;
        vec![entry]
    } else {
        let project = bougie_config::load_project(&project_root)?;
        catalog::CATALOG
            .iter()
            .filter(|e| e.user_facing && project.bougie.services.contains_key(e.name))
            .collect()
    };

    let mut services = Vec::new();
    let mut not_provisioned = Vec::new();
    for entry in entries {
        let Some(mut tenant) = find_tenant(&paths, entry.name, &project_root)? else {
            if name.is_some() {
                return Err(no_tenant_err(entry.name));
            }
            not_provisioned.push(entry.name.to_string());
            continue;
        };
        resolve_password(&paths, entry, &mut tenant)?;
        let vars = tenant_service_env(&paths, entry, &tenant);
        services.push(ServiceCredentials {
            service: entry.name.to_string(),
            tenant: tenant.tenant,
            connection: friendly_map(entry.name, &vars),
            vars,
        });
    }

    let result = CredentialsResult {
        schema_version: 1,
        project: project_root.display().to_string(),
        services,
        not_provisioned,
    };
    if env {
        print_env(&result)?;
    } else {
        emit(format, &result)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// The daemon's provisioners persist the password into the ledger row;
/// rows provisioned before that existed may lack it. Fall back to
/// deriving it — the same function the provisioners use, so the values
/// agree (mirrors the mariadb wiring in `exec.rs`).
fn resolve_password(paths: &Paths, entry: &CatalogEntry, tenant: &mut Tenant) -> Result<()> {
    if !matches!(entry.tenancy, Tenancy::Mariadb | Tenancy::Rabbitmq)
        || tenant.secrets.contains_key("password")
    {
        return Ok(());
    }
    let pw = derive_password(paths, entry.name, &tenant.project)?;
    tenant.secrets.insert("password".into(), pw);
    Ok(())
}

/// Strip `BOUGIE_SERVICE_<NAME>_` and lowercase, so `…_DASHBOARD_URL`
/// reads as `dashboard_url` in the text/JSON views.
fn friendly_map(service: &str, vars: &serde_json::Map<String, Value>) -> BTreeMap<String, Value> {
    let prefix = format!("BOUGIE_SERVICE_{}_", service.to_ascii_uppercase());
    vars.iter()
        .map(|(k, v)| {
            let key = k.strip_prefix(&prefix).unwrap_or(k).to_ascii_lowercase();
            (key, v.clone())
        })
        .collect()
}

/// `KEY='value'` lines under the canonical env names, one per var, so
/// `eval "$(bougie service credentials --env)"` reproduces the
/// `bougie run` environment. Not-provisioned notes become `#` comments
/// to stay eval-safe.
fn print_env(result: &CredentialsResult) -> io::Result<()> {
    let stdout = io::stdout();
    let mut w = stdout.lock();
    for n in &result.not_provisioned {
        writeln!(w, "# {n}: not provisioned — run `bougie up {n}` first")?;
    }
    for s in &result.services {
        for (k, v) in &s.vars {
            writeln!(w, "{k}={}", shell_quote(&value_str(v)))?;
        }
    }
    Ok(())
}

fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Single-quote for POSIX shells (a literal `'` becomes `'\''`). The
/// values are mostly hex and URLs, but socket paths inherit
/// `BOUGIE_HOME`, which may contain spaces or quotes.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mariadb_vars() -> serde_json::Map<String, Value> {
        let mut vars = serde_json::Map::new();
        vars.insert(
            "BOUGIE_SERVICE_MARIADB_DATABASE".into(),
            Value::String("acme".into()),
        );
        vars.insert(
            "BOUGIE_SERVICE_MARIADB_PASSWORD".into(),
            Value::String("deadbeef".into()),
        );
        vars
    }

    #[test]
    fn friendly_map_strips_prefix_and_lowercases() {
        let friendly = friendly_map("mariadb", &mariadb_vars());
        assert_eq!(friendly["database"], Value::String("acme".into()));
        assert_eq!(friendly["password"], Value::String("deadbeef".into()));
    }

    #[test]
    fn shell_quote_handles_embedded_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn render_text_blocks_and_not_provisioned_notes() {
        let vars = mariadb_vars();
        let result = CredentialsResult {
            schema_version: 1,
            project: "/p/acme".into(),
            services: vec![ServiceCredentials {
                service: "mariadb".into(),
                tenant: "acme".into(),
                connection: friendly_map("mariadb", &vars),
                vars,
            }],
            not_provisioned: vec!["redis".into()],
        };
        let mut out = Vec::new();
        result.render_text(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("mariadb (tenant acme)"), "{text}");
        assert!(text.contains("password  deadbeef"), "{text}");
        assert!(
            text.contains("redis: not provisioned — run `bougie up redis` first"),
            "{text}"
        );
    }

    #[test]
    fn resolve_password_fallback_matches_derivation() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(dir.path().to_path_buf(), dir.path().join("cache"));
        let entry = catalog::find("mariadb").unwrap();

        let mut tenant = Tenant::new("acme", "/p/acme");
        resolve_password(&paths, entry, &mut tenant).unwrap();
        let derived = derive_password(&paths, "mariadb", std::path::Path::new("/p/acme")).unwrap();
        assert_eq!(tenant.secrets["password"], derived);

        // A ledger-recorded password wins over derivation.
        let mut tenant = Tenant::new("acme", "/p/acme");
        tenant.secrets.insert("password".into(), "ledger".into());
        resolve_password(&paths, entry, &mut tenant).unwrap();
        assert_eq!(tenant.secrets["password"], "ledger");

        // Passwordless services are left untouched.
        let redis = catalog::find("redis").unwrap();
        let mut tenant = Tenant::new("acme", "/p/acme");
        resolve_password(&paths, redis, &mut tenant).unwrap();
        assert!(tenant.secrets.is_empty());
    }
}
