use crate::cli::OutputFormat;
use crate::composer::fetch::{build_client, fetch_channels};
use crate::output::{emit_paged, Render};
use crate::paths::Paths;
use eyre::{Result, WrapErr};
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ListResult {
    pub schema_version: u32,
    pub installed: Vec<String>,
    pub stable: Vec<String>,
    pub preview: Vec<String>,
}

impl Render for ListResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.installed.is_empty() {
            writeln!(w, "no composer versions installed")?;
        } else {
            for v in &self.installed {
                writeln!(w, "installed  {v}")?;
            }
        }
        for v in &self.stable {
            writeln!(w, "available  {v} (stable)")?;
        }
        for v in &self.preview {
            writeln!(w, "available  {v} (preview)")?;
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat, field: Option<&str>) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let installed = list_installed(&paths)?;

    // The available view is best-effort: failure to reach getcomposer.org
    // (or a stale cache) shouldn't make `bougie composer list` unusable.
    let (stable, preview) = fetch_available(&paths).unwrap_or((Vec::new(), Vec::new()));

    let result = ListResult { schema_version: 1, installed, stable, preview };
    emit_paged(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn list_installed(paths: &Paths) -> Result<Vec<String>> {
    let root = paths.composer_root();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out: BTreeSet<String> = BTreeSet::new();
    for entry in std::fs::read_dir(&root).wrap_err_with(|| format!("reading {}", root.display()))? {
        let entry = entry.wrap_err("dir entry")?;
        // Cache sidecars (channels.json, channels.json.etag) are flat
        // files; only directories represent installed versions.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let phar = entry.path().join("composer.phar");
        if phar.is_file() {
            out.insert(name.to_owned());
        }
    }
    Ok(out.into_iter().collect())
}

fn fetch_available(paths: &Paths) -> Result<(Vec<String>, Vec<String>)> {
    let client = build_client()?;
    let channels = fetch_channels(&client, paths)?;
    Ok((
        channels.stable.into_iter().map(|e| e.version).collect(),
        channels.preview.into_iter().map(|e| e.version).collect(),
    ))
}
