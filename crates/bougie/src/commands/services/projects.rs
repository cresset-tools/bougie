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

use super::client;
use bougie_cli::OutputFormat;
use bougie_daemon::daemon::catalog::{self, Tenancy};
use bougie_daemon::daemon::tenants::Tenant;
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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
    /// True when the project directory no longer exists on disk — the
    /// tenant is provisioned but orphaned (the project was moved or
    /// deleted without `bougie down --purge`).
    #[serde(skip_serializing_if = "is_false")]
    pub missing: bool,
    /// RFC 3339 provisioning timestamp.
    pub created_at: String,
    /// Provisioner-specific allocation (redis `db_number`, rabbitmq
    /// `vhost`, server `hostname`, …). For mariadb — whose database and
    /// user are simply the tenant name and so aren't stored in the
    /// ledger — `database`/`user` are synthesized here for display.
    /// Secrets are deliberately excluded.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub alloc: BTreeMap<String, Value>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde skip predicate signature
fn is_false(b: &bool) -> bool {
    !*b
}

impl Render for ProjectsResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.tenants.is_empty() {
            writeln!(w, "No provisioned tenants.")?;
            return Ok(());
        }

        let home = std::env::var_os("HOME").map(PathBuf::from);
        // Project cell = abbreviated path, with a ` (missing)` marker
        // when the directory is gone.
        let project_cell = |row: &TenantRow| -> String {
            let base = abbreviate_home(&row.project, home.as_deref());
            if row.missing {
                format!("{base} (missing)")
            } else {
                base
            }
        };

        // Dynamic column widths so long project paths still align.
        let mut w_service = "SERVICE".len();
        let mut w_tenant = "TENANT".len();
        let mut w_project = "PROJECT".len();
        for row in &self.tenants {
            w_service = w_service.max(row.service.len());
            w_tenant = w_tenant.max(row.tenant.len());
            w_project = w_project.max(project_cell(row).len());
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
                project_cell(row),
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
    let mut tenants = load_rows(&paths)?;

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

/// Read every provisioned tenant across the multi-tenant services into
/// display rows (mariadb db/user synthesized, missing-dir flag set).
/// Shared by the list view ([`run`]) and [`purge`].
fn load_rows(paths: &Paths) -> Result<Vec<TenantRow>> {
    let mut tenants: Vec<TenantRow> = Vec::new();
    for entry in catalog::CATALOG {
        // Runtime-only deps (jdk, erlang) have no tenant ledger.
        if matches!(entry.tenancy, Tenancy::None) {
            continue;
        }
        for t in load_ledger(&paths.service_tenants(entry.name))? {
            let mut alloc = t.alloc;
            // mariadb's database + user are just the tenant name and so
            // aren't recorded in the ledger's `alloc`; synthesize them so
            // `--alloc` shows where the project's data actually lives.
            if entry.name == "mariadb" {
                alloc
                    .entry("database".to_string())
                    .or_insert_with(|| Value::String(t.tenant.clone()));
                alloc
                    .entry("user".to_string())
                    .or_insert_with(|| Value::String(t.tenant.clone()));
            }
            let missing = !t.project.exists();
            tenants.push(TenantRow {
                service: entry.name.to_string(),
                tenant: t.tenant,
                project: t.project,
                missing,
                created_at: t.created_at,
                alloc,
            });
        }
    }
    Ok(tenants)
}

/// One project's purge outcome (plan in `--dry-run`, result otherwise).
#[derive(Debug, Serialize)]
pub struct PurgedProject {
    pub project: PathBuf,
    #[serde(skip_serializing_if = "is_false")]
    pub missing: bool,
    /// Services deprovisioned for this project.
    pub services: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PurgeResult {
    pub schema_version: u32,
    pub dry_run: bool,
    pub purged: Vec<PurgedProject>,
}

impl Render for PurgeResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.purged.is_empty() {
            writeln!(w, "nothing to purge")?;
            return Ok(());
        }
        let verb = if self.dry_run { "would purge" } else { "purged" };
        for p in &self.purged {
            let miss = if p.missing { " (missing)" } else { "" };
            writeln!(
                w,
                "{verb} {}{miss}: {}",
                p.project.display(),
                p.services.join(", "),
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct DownReply {
    #[serde(default)]
    deprovisioned: Vec<String>,
}

/// `bougie services projects purge` — deprovision tenants and remove
/// them from the ledgers. Default target is the *orphaned* set (project
/// dir gone); `--project <path>` targets one project, `--all` targets
/// everything. Destructive (with the service running it drops the
/// tenant's data), so it confirms unless `--yes`/`--dry-run`.
pub fn purge(
    format: OutputFormat,
    project: Option<String>,
    all: bool,
    dry_run: bool,
    yes: bool,
) -> Result<ExitCode> {
    use std::collections::BTreeSet;
    use std::io::IsTerminal;

    let paths = Paths::from_env()?;
    let rows = load_rows(&paths)?;

    let targets: Vec<&TenantRow> = if all {
        rows.iter().collect()
    } else if let Some(p) = project {
        // Match the ledger's canonical path, but also accept the raw
        // form the user typed (the dir may be gone → can't canonicalize).
        let canon = std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(&p));
        let raw = PathBuf::from(p);
        rows.iter()
            .filter(|r| r.project == canon || r.project == raw)
            .collect()
    } else {
        rows.iter().filter(|r| r.missing).collect()
    };

    // Group by project → (missing, set of services).
    let mut by_project: BTreeMap<PathBuf, (bool, BTreeSet<String>)> = BTreeMap::new();
    for r in &targets {
        let e = by_project
            .entry(r.project.clone())
            .or_insert((r.missing, BTreeSet::new()));
        e.1.insert(r.service.clone());
    }

    if by_project.is_empty() {
        emit(format, &PurgeResult { schema_version: 1, dry_run, purged: Vec::new() })?;
        return Ok(ExitCode::SUCCESS);
    }

    if dry_run {
        let plan = by_project
            .iter()
            .map(|(proj, (missing, svcs))| PurgedProject {
                project: proj.clone(),
                missing: *missing,
                services: svcs.iter().cloned().collect(),
            })
            .collect();
        emit(format, &PurgeResult { schema_version: 1, dry_run: true, purged: plan })?;
        return Ok(ExitCode::SUCCESS);
    }

    // Destructive: confirm unless told otherwise.
    if !yes {
        let interactive = matches!(format, OutputFormat::Text) && std::io::stdin().is_terminal();
        if !interactive {
            return Err(eyre!(
                "refusing to purge {} tenant(s) across {} project(s) without confirmation; \
                 re-run with --yes",
                targets.len(),
                by_project.len(),
            ));
        }
        eprint!(
            "Purge {} tenant(s) across {} project(s)? This destroys their data. [y/N] ",
            targets.len(),
            by_project.len(),
        );
        io::Write::flush(&mut io::stderr()).ok();
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| eyre!("reading confirmation: {e}"))?;
        let ans = line.trim().to_ascii_lowercase();
        if ans != "y" && ans != "yes" {
            eprintln!("aborted; nothing purged.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    // Deprovision each project's services via the daemon. `service.down`
    // matches the tenant by project path (works for already-deleted
    // dirs) and removes the ledger entry.
    let mut purged: Vec<PurgedProject> = Vec::new();
    for (proj, (missing, svcs)) in &by_project {
        let services: Vec<&String> = svcs.iter().collect();
        let args = json!({ "project": proj, "services": services, "purge": true });
        let reply: DownReply = client::call(&paths, "service.down", args)?;
        let mut done = reply.deprovisioned;
        done.sort();
        purged.push(PurgedProject { project: proj.clone(), missing: *missing, services: done });
    }
    emit(format, &PurgeResult { schema_version: 1, dry_run: false, purged })?;
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
            missing: false,
            created_at: tenants[0].created_at.clone(),
            alloc: tenants[0].alloc.clone(),
        };
        let json = serde_json::to_string(&row).unwrap();
        assert!(!json.to_lowercase().contains("secret"), "secrets must never serialize: {json}");
        assert!(json.contains("db_number"));
        // `missing` is false → omitted from JSON.
        assert!(!json.contains("missing"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_marks_missing_projects_and_serializes_flag() {
        let row = TenantRow {
            service: "redis".into(),
            tenant: "acme".into(),
            project: "/gone/acme".into(),
            missing: true,
            created_at: "2026-06-05T00:00:00Z".into(),
            alloc: BTreeMap::new(),
        };
        let result = ProjectsResult { schema_version: 1, tenants: vec![row], show_alloc: false };
        let mut buf = Vec::new();
        result.render_text(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("/gone/acme (missing)"), "text: {text}");
        // A missing flag *does* serialize to JSON (unlike the false case).
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"missing\":true"), "json: {json}");
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
