//! `bougie composer fund` — show funding information for installed
//! packages, grouped by vendor (Composer's layout). Reads
//! `composer.lock` and the typed `LockPackage.funding` field (Phase-0
//! `funding` helper).

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer::lockfile::Lock;
use bougie_composer_resolver::funding;
use bougie_output::output::{emit, Render};
use eyre::{eyre, Context, Result};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct FundEntry {
    pub package: String,
    /// `"type: url"` pairs, in declared order.
    pub urls: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct FundResult {
    pub schema_version: u32,
    /// Vendor → its funded packages.
    pub vendors: BTreeMap<String, Vec<FundEntry>>,
}

impl Render for FundResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.vendors.is_empty() {
            return writeln!(
                w,
                "No funding links were found in your package dependencies. \
                 This doesn't mean they don't need your support!"
            );
        }
        writeln!(
            w,
            "The following packages were found in your dependencies which publish funding information:\n"
        )?;
        for (vendor, entries) in &self.vendors {
            writeln!(w, "{vendor}")?;
            for e in entries {
                writeln!(w, "  {}", e.package)?;
                for url in &e.urls {
                    writeln!(w, "    {url}")?;
                }
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

    let pkgs: Vec<_> = if no_dev {
        lock.packages.iter().collect()
    } else {
        lock.all_packages().collect()
    };

    let mut vendors: BTreeMap<String, Vec<FundEntry>> = BTreeMap::new();
    for p in &pkgs {
        let funds = funding(p);
        if funds.is_empty() {
            continue;
        }
        let vendor = p.name.split_once('/').map_or(p.name.as_str(), |(v, _)| v).to_string();
        let urls = funds
            .iter()
            .map(|f| {
                if f.kind.is_empty() {
                    f.url.clone()
                } else {
                    format!("{}: {}", f.kind, f.url)
                }
            })
            .collect();
        vendors.entry(vendor).or_default().push(FundEntry {
            package: p.name.clone(),
            urls,
        });
    }
    for entries in vendors.values_mut() {
        entries.sort_by(|a, b| a.package.cmp(&b.package));
    }

    emit(format, &FundResult { schema_version: 1, vendors })?;
    Ok(ExitCode::SUCCESS)
}
