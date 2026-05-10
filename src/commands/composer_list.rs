use crate::cli::OutputFormat;
use crate::composer::fetch::{build_client, fetch_channels, ChannelEntry};
use crate::output::{emit, Render};
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
    pub stable_latest: Option<String>,
    pub preview_latest: Option<String>,
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
        if let Some(s) = &self.stable_latest {
            writeln!(w, "available  {s} (stable)")?;
        }
        if let Some(p) = &self.preview_latest {
            writeln!(w, "available  {p} (preview)")?;
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat, field: Option<&str>) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let installed = list_installed(&paths)?;

    // The available view is best-effort: failure to reach getcomposer.org
    // (or a stale cache) shouldn't make `bougie composer list` unusable.
    let (stable_latest, preview_latest) = fetch_available(&paths).unwrap_or((None, None));

    let result = ListResult {
        schema_version: 1,
        installed,
        stable_latest,
        preview_latest,
    };
    emit(format, field, &result)?;
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

fn fetch_available(paths: &Paths) -> Result<(Option<String>, Option<String>)> {
    let client = build_client()?;
    let channels = fetch_channels(&client, paths)?;
    Ok((
        first_version(&channels.stable),
        first_version(&channels.preview),
    ))
}

fn first_version(entries: &[ChannelEntry]) -> Option<String> {
    entries.iter().next().map(|e| e.version.clone())
}
