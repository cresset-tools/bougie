//! `bougie composer licenses` — list the license of every installed
//! package, plus a summary count. Reads `composer.lock` and the typed
//! `LockPackage.license` field (Phase-0 `licenses` helper).

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer::lockfile::Lock;
use bougie_composer_resolver::licenses;
use bougie_output::output::{emit, Render};
use eyre::{eyre, Context, Result};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct LicenseRow {
    pub name: String,
    pub version: String,
    pub license: String,
}

#[derive(Debug, Serialize)]
pub struct LicensesResult {
    pub schema_version: u32,
    /// Root package name + its declared license, for the header.
    pub project: String,
    pub project_license: String,
    pub dependencies: Vec<LicenseRow>,
    /// License → number of dependencies under it.
    pub summary: BTreeMap<String, usize>,
}

impl Render for LicensesResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "Name: {}", self.project)?;
        writeln!(w, "License: {}", self.project_license)?;
        writeln!(w, "Dependencies:\n")?;
        if self.dependencies.is_empty() {
            writeln!(w, "(none)")?;
            return Ok(());
        }
        let name_w = self.dependencies.iter().map(|r| r.name.len()).max().unwrap_or(0);
        let ver_w = self.dependencies.iter().map(|r| r.version.len()).max().unwrap_or(0);
        for r in &self.dependencies {
            writeln!(w, "{:name_w$}  {:ver_w$}  {}", r.name, r.version, r.license)?;
        }
        writeln!(w, "\nSummary:")?;
        for (lic, n) in &self.summary {
            writeln!(w, "  {lic}: {n}")?;
        }
        Ok(())
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "wired from clap-parsed CLI; ownership crosses the boundary"
)]
pub fn run(
    format: OutputFormat,
    no_dev: bool,
    working_dir: Option<PathBuf>,
) -> Result<ExitCode> {
    let project_root = match working_dir {
        Some(p) => p,
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
    let root = std::fs::read(project_root.join("composer.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .unwrap_or(serde_json::Value::Null);

    let pkgs: Vec<_> = if no_dev {
        lock.packages.iter().collect()
    } else {
        lock.all_packages().collect()
    };

    let mut dependencies = Vec::with_capacity(pkgs.len());
    let mut summary: BTreeMap<String, usize> = BTreeMap::new();
    for p in &pkgs {
        let lic = join_licenses(licenses(p));
        *summary.entry(lic.clone()).or_default() += 1;
        dependencies.push(LicenseRow {
            name: p.name.clone(),
            version: p.version.clone(),
            license: lic,
        });
    }
    dependencies.sort_by(|a, b| a.name.cmp(&b.name));

    let result = LicensesResult {
        schema_version: 1,
        project: root
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("__root__")
            .to_string(),
        project_license: root
            .get("license")
            .and_then(license_to_string)
            .unwrap_or_else(|| "none".to_string()),
        dependencies,
        summary,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// Join a package's SPDX identifiers; empty → `"none"` (Composer's
/// placeholder for an undeclared license).
fn join_licenses(list: &[String]) -> String {
    if list.is_empty() {
        "none".to_string()
    } else {
        list.join(", ")
    }
}

/// composer.json's `license` is either a string or an array of strings.
fn license_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(a) => {
            let parts: Vec<String> = a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        _ => None,
    }
}
