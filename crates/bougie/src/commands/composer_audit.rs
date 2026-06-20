//! `bougie composer audit` — check installed packages against the
//! Packagist security-advisories database.
//!
//! Posts the locked package names to the advisories API (via
//! `bougie_composer_resolver::audit`), matches each advisory's
//! `affectedVersions` constraint against the locked version with
//! `composer-semver`, and reports the hits. Exits non-zero when any
//! advisory matches — CI-friendly.
//!
//! `--abandoned` is accepted for parity but abandoned-package detection
//! isn't wired yet (the lockfile doesn't carry the `abandoned` flag);
//! it's a documented follow-up.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use bougie_cli::{AbandonedHandling, OutputFormat};
use bougie_composer::lockfile::Lock;
use bougie_composer_resolver::audit::{self, Advisory};
use bougie_composer_resolver::metadata::build_client;
use bougie_output::output::{emit, Render};
use composer_semver::constraint::Constraint;
use composer_semver::version::Version;
use eyre::{eyre, Context, Result};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct AuditOptions {
    pub no_dev: bool,
    pub abandoned: AbandonedHandling,
    pub locked: bool,
    pub working_dir: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct AuditFinding {
    pub package: String,
    pub version: String,
    pub advisory_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cve: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    pub affected_versions: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuditResult {
    pub schema_version: u32,
    pub packages_audited: usize,
    pub findings: Vec<AuditFinding>,
}

impl Render for AuditResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.findings.is_empty() {
            return writeln!(
                w,
                "No security vulnerability advisories found ({} packages audited).",
                self.packages_audited
            );
        }
        writeln!(
            w,
            "Found {} security vulnerability advisory(ies):\n",
            self.findings.len()
        )?;
        for f in &self.findings {
            writeln!(w, "{} ({})", f.package, f.version)?;
            writeln!(w, "  advisory : {}", f.advisory_id)?;
            writeln!(w, "  title    : {}", f.title)?;
            if let Some(cve) = &f.cve
                && !cve.is_empty()
            {
                writeln!(w, "  CVE      : {cve}")?;
            }
            if let Some(sev) = &f.severity
                && !sev.is_empty()
            {
                writeln!(w, "  severity : {sev}")?;
            }
            writeln!(w, "  affected : {}", f.affected_versions)?;
            if let Some(link) = &f.link
                && !link.is_empty()
            {
                writeln!(w, "  link     : {link}")?;
            }
            writeln!(w)?;
        }
        Ok(())
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "wired from clap-parsed CLI; ownership crosses the boundary"
)]
pub fn run(format: OutputFormat, opts: AuditOptions) -> Result<ExitCode> {
    let _ = (opts.abandoned, opts.locked); // accepted for parity; see module docs.
    let project_root = match &opts.working_dir {
        Some(p) => p.clone(),
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };

    let lock_path = project_root.join("composer.lock");
    if !lock_path.is_file() {
        return Err(eyre!(
            "no composer.lock in {} — run `bougie composer install` or `update` first",
            project_root.display()
        ));
    }
    let lock = Lock::read(&lock_path).wrap_err_with(|| format!("reading {}", lock_path.display()))?;

    // Locked version per package name (lowercased), for constraint
    // matching against advisories.
    let mut versions: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let pkgs: Vec<_> = if opts.no_dev {
        lock.packages.iter().collect()
    } else {
        lock.all_packages().collect()
    };
    for p in &pkgs {
        versions.insert(p.name.to_ascii_lowercase(), (p.name.clone(), p.version.clone()));
    }
    let names: Vec<String> = pkgs.iter().map(|p| p.name.clone()).collect();
    let packages_audited = names.len();

    let client = build_client()?;
    let advisories = audit::fetch_advisories(&client, &audit::base_url(), &names)
        .wrap_err("fetching security advisories")?;

    let mut findings = Vec::new();
    for (pkg_name, list) in &advisories {
        let key = pkg_name.to_ascii_lowercase();
        let Some((display_name, version_str)) = versions.get(&key) else {
            continue;
        };
        let Ok(version) = Version::parse(version_str) else {
            continue;
        };
        for adv in list {
            if advisory_matches(adv, &version) {
                findings.push(AuditFinding {
                    package: display_name.clone(),
                    version: version_str.clone(),
                    advisory_id: adv.advisory_id.clone(),
                    title: adv.title.clone(),
                    cve: adv.cve.clone(),
                    severity: adv.severity.clone(),
                    affected_versions: adv.affected_versions.clone(),
                    link: adv.link.clone(),
                });
            }
        }
    }
    findings.sort_by(|a, b| {
        a.package
            .cmp(&b.package)
            .then_with(|| a.advisory_id.cmp(&b.advisory_id))
    });

    let any = !findings.is_empty();
    emit(
        format,
        &AuditResult {
            schema_version: 1,
            packages_audited,
            findings,
        },
    )?;
    Ok(if any { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

/// Does this advisory's `affectedVersions` constraint cover the locked
/// version? A constraint that fails to parse is treated as a match (fail
/// safe — surface the advisory rather than silently dropping it).
fn advisory_matches(adv: &Advisory, version: &Version) -> bool {
    match Constraint::parse(&adv.affected_versions) {
        Ok(c) => c.matches(version),
        Err(_) => true,
    }
}
