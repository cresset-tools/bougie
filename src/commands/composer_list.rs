use crate::cli::OutputFormat;
use crate::composer::fetch::{build_client, fetch_channels};
use crate::list_format::{write_row, KeyParts, Suffix};
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
        // Always surface the "nothing installed" hint when the on-disk
        // set is empty — even if the available channels are populated.
        // Hides the difference between "no network" and "nothing
        // downloaded yet" behind one familiar line.
        if self.installed.is_empty() {
            writeln!(w, "no composer versions installed")?;
        }
        if self.installed.is_empty() && self.stable.is_empty() && self.preview.is_empty() {
            return Ok(());
        }

        // Resolve installed phar paths lazily so the JSON schema stays
        // version-only. The path is only needed for the green
        // text-mode suffix.
        let paths = Paths::from_env().ok();
        let installed_set: BTreeSet<&str> = self.installed.iter().map(String::as_str).collect();

        let mut pad = 0;
        for v in &self.installed {
            pad = pad.max(KeyParts { version: v, ..KeyParts::default() }.plain_len());
        }
        for v in self.stable.iter().chain(self.preview.iter()) {
            if installed_set.contains(v.as_str()) {
                continue;
            }
            // Stable and preview share the same key shape; only the
            // channel name differs, and both are 6/7 chars — measure
            // once with the longer.
            pad = pad.max(channel_key(v, "preview").plain_len());
        }

        for v in &self.installed {
            let key = KeyParts { version: v, ..KeyParts::default() };
            match &paths {
                Some(p) => {
                    let phar = p.composer_phar(v);
                    write_row(w, &key, pad, &Suffix::Path(&phar), None)?;
                }
                // Falling back to the placeholder keeps the column
                // alignment if $BOUGIE_HOME resolution somehow fails
                // — better than skipping the row.
                None => write_row(w, &key, pad, &Suffix::Placeholder, None)?,
            }
        }
        for v in &self.stable {
            if installed_set.contains(v.as_str()) {
                continue;
            }
            write_row(w, &channel_key(v, "stable"), pad, &Suffix::Placeholder, None)?;
        }
        for v in &self.preview {
            if installed_set.contains(v.as_str()) {
                continue;
            }
            write_row(w, &channel_key(v, "preview"), pad, &Suffix::Placeholder, None)?;
        }
        Ok(())
    }
}

fn channel_key<'a>(version: &'a str, channel: &'a str) -> KeyParts<'a> {
    KeyParts {
        prefix: None,
        version,
        target: None,
        flavor: Some(channel),
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
