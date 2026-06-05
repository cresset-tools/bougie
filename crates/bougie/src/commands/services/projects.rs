//! `bougie services projects` — list every provisioned tenant across
//! the shared services and the project each belongs to.
//!
//! Reads the on-disk tenant ledgers
//! (`$BOUGIE_HOME/state/services/<svc>/tenants.json`, see SERVICES.md
//! §3.3) directly — no daemon round-trip, so it works even when
//! `bougied` is down. The ledger is the source of truth for what's
//! provisioned; the daemon only ever appends to it.
//!
//! Secrets in the ledger (mariadb/rabbitmq passwords) are never
//! emitted — only the tenant name, owning project, creation time, and
//! (with `--alloc`) the non-sensitive allocation map.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_daemon::daemon::catalog::{self, Tenancy};
use bougie_daemon::daemon::tenants::Tenant;
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use eyre::{eyre, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct ProjectsResult {
    pub schema_version: u32,
    pub tenants: Vec<TenantRow>,
    /// Echoed so the text renderer knows whether to draw the ALLOC
    /// column; skipped from JSON (the `alloc` map is always present
    /// there regardless).
    #[serde(skip)]
    pub show_alloc: bool,
}

#[derive(Debug, Serialize)]
pub struct TenantRow {
    /// Catalog service name (redis, mariadb, …).
    pub service: String,
    /// Public tenant name inside the service (DB name, vhost, …).
    pub tenant: String,
    /// Absolute path of the owning project.
    pub project: PathBuf,
    /// RFC 3339 provisioning timestamp.
    pub created_at: String,
    /// Provisioner-specific allocation (redis `db_number`, rabbitmq
    /// `vhost`, server `hostname`, …). Secrets are deliberately excluded.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub alloc: BTreeMap<String, Value>,
}

impl Render for ProjectsResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.tenants.is_empty() {
            writeln!(w, "No provisioned tenants.")?;
            return Ok(());
        }

        let home = std::env::var_os("HOME").map(PathBuf::from);
        let display_project = |p: &Path| -> String { abbreviate_home(p, home.as_deref()) };

        // Dynamic column widths so long project paths still align.
        let mut w_service = "SERVICE".len();
        let mut w_tenant = "TENANT".len();
        let mut w_project = "PROJECT".len();
        for row in &self.tenants {
            w_service = w_service.max(row.service.len());
            w_tenant = w_tenant.max(row.tenant.len());
            w_project = w_project.max(display_project(&row.project).len());
        }

        write!(
            w,
            "{:<ws$}  {:<wt$}  {:<wp$}  CREATED",
            "SERVICE",
            "TENANT",
            "PROJECT",
            ws = w_service,
            wt = w_tenant,
            wp = w_project,
        )?;
        if self.show_alloc {
            write!(w, "  ALLOC")?;
        }
        writeln!(w)?;

        for row in &self.tenants {
            write!(
                w,
                "{:<ws$}  {:<wt$}  {:<wp$}  {}",
                row.service,
                row.tenant,
                display_project(&row.project),
                format_created(&row.created_at),
                ws = w_service,
                wt = w_tenant,
                wp = w_project,
            )?;
            if self.show_alloc {
                write!(w, "  {}", format_alloc(&row.alloc))?;
            }
            writeln!(w)?;
        }
        Ok(())
    }
}

/// Replace a leading `$HOME` with `~` for compact display.
fn abbreviate_home(p: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && let Ok(rest) = p.strip_prefix(home)
    {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rest.display());
    }
    p.display().to_string()
}

/// `2026-06-05T12:34:56.7+00:00` → `2026-06-05 12:34`. Defensive: any
/// unexpected shape falls back to the raw string.
fn format_created(s: &str) -> String {
    match s.get(0..16) {
        Some(prefix) if prefix.contains('T') => prefix.replacen('T', " ", 1),
        _ => s.to_string(),
    }
}

/// `{"db_number": 3}` → `db_number=3`; `{"hostname": "x.bougie.run"}` →
/// `hostname=x.bougie.run`. Strings render unquoted; everything else
/// uses its JSON form.
fn format_alloc(alloc: &BTreeMap<String, Value>) -> String {
    alloc
        .iter()
        .map(|(k, v)| {
            let val = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{k}={val}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Read one service's tenant ledger synchronously. Mirrors
/// `bougie_daemon::daemon::tenants::load_all` (which is async); a
/// missing file means the service was never provisioned for any
/// project and yields an empty list.
fn load_ledger(path: &Path) -> Result<Vec<Tenant>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(eyre!("reading {}: {e}", path.display())),
    };
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let t: Tenant = serde_json::from_str(line)
            .map_err(|e| eyre!("parsing line {} of {}: {e}", i + 1, path.display()))?;
        out.push(t);
    }
    Ok(out)
}

pub fn run(format: OutputFormat, show_alloc: bool) -> Result<ExitCode> {
    let paths = Paths::from_env()?;

    let mut tenants: Vec<TenantRow> = Vec::new();
    for entry in catalog::CATALOG {
        // Runtime-only deps (jdk, erlang) have no tenant ledger.
        if matches!(entry.tenancy, Tenancy::None) {
            continue;
        }
        let ledger = paths.service_tenants(entry.name);
        for t in load_ledger(&ledger)? {
            tenants.push(TenantRow {
                service: entry.name.to_string(),
                tenant: t.tenant,
                project: t.project,
                created_at: t.created_at,
                alloc: t.alloc,
            });
        }
    }

    // Group by service, then project, then tenant — stable + predictable.
    tenants.sort_by(|a, b| {
        a.service
            .cmp(&b.service)
            .then_with(|| a.project.cmp(&b.project))
            .then_with(|| a.tenant.cmp(&b.tenant))
    });

    let result = ProjectsResult {
        schema_version: 1,
        tenants,
        show_alloc,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_created_trims_rfc3339_to_minute() {
        assert_eq!(format_created("2026-06-05T12:34:56.7+00:00"), "2026-06-05 12:34");
        assert_eq!(format_created("2026-06-05T12:34:00Z"), "2026-06-05 12:34");
        // Unexpected shape falls back to the raw string.
        assert_eq!(format_created("whenever"), "whenever");
        assert_eq!(format_created(""), "");
    }

    #[test]
    fn abbreviate_home_replaces_leading_home() {
        let home = Path::new("/home/jelle");
        assert_eq!(abbreviate_home(Path::new("/home/jelle/work/acme"), Some(home)), "~/work/acme");
        // Exact home → bare tilde.
        assert_eq!(abbreviate_home(Path::new("/home/jelle"), Some(home)), "~");
        // Outside home is left untouched.
        assert_eq!(abbreviate_home(Path::new("/opt/x"), Some(home)), "/opt/x");
        // No HOME set → untouched.
        assert_eq!(abbreviate_home(Path::new("/home/jelle/x"), None), "/home/jelle/x");
    }

    #[test]
    fn format_alloc_renders_strings_unquoted_and_sorts_keys() {
        let mut alloc = BTreeMap::new();
        alloc.insert("vhost".to_string(), Value::String("acme".to_string()));
        alloc.insert("db_number".to_string(), Value::from(3));
        // BTreeMap iterates sorted: db_number before vhost.
        assert_eq!(format_alloc(&alloc), "db_number=3 vhost=acme");
        assert_eq!(format_alloc(&BTreeMap::new()), "");
    }

    #[test]
    fn load_ledger_parses_lines_and_drops_secrets_at_row_level() {
        let dir = std::env::temp_dir().join(format!("bougie-projects-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tenants.json");
        std::fs::write(
            &path,
            "{\"schema_version\":1,\"tenant\":\"acme\",\"project\":\"/p/acme\",\"created_at\":\"2026-06-05T00:00:00Z\",\"secrets\":{\"password\":\"SECRET\"},\"alloc\":{\"db_number\":2}}\n\n",
        )
        .unwrap();

        let tenants = load_ledger(&path).unwrap();
        assert_eq!(tenants.len(), 1, "blank line skipped");
        // The ledger Tenant still carries secrets…
        assert!(tenants[0].secrets.contains_key("password"));
        // …but the TenantRow that gets serialized has no secrets field at all.
        let row = TenantRow {
            service: "redis".into(),
            tenant: tenants[0].tenant.clone(),
            project: tenants[0].project.clone(),
            created_at: tenants[0].created_at.clone(),
            alloc: tenants[0].alloc.clone(),
        };
        let json = serde_json::to_string(&row).unwrap();
        assert!(!json.to_lowercase().contains("secret"), "secrets must never serialize: {json}");
        assert!(json.contains("db_number"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_ledger_missing_file_is_empty() {
        let tenants = load_ledger(Path::new("/no/such/tenants.json")).unwrap();
        assert!(tenants.is_empty());
    }

    #[test]
    fn load_ledger_rejects_malformed_line() {
        let dir = std::env::temp_dir().join(format!("bougie-projects-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tenants.json");
        std::fs::write(&path, "{not json}\n").unwrap();
        assert!(load_ledger(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
