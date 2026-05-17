//! `bougie composer upgrade` — refresh the locally-installed `stable`
//! and `preview` channel heads to whatever getcomposer.org currently
//! advertises. Existing exact-version installs are not touched.

use crate::cli::OutputFormat;
use crate::composer::{install_composer, request::Channel, ComposerRequest, Installed};
use crate::output::{emit, Render};
use crate::paths::Paths;
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct UpgradeResult {
    pub schema_version: u32,
    pub installed: Vec<UpgradeRow>,
}

#[derive(Debug, Serialize)]
pub struct UpgradeRow {
    pub channel: &'static str,
    pub version: String,
    pub path: PathBuf,
    pub already_present: bool,
}

impl Render for UpgradeResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for r in &self.installed {
            let verb = if r.already_present { "already" } else { "installed" };
            writeln!(
                w,
                "{verb}  composer {} ({})  -> {}",
                r.version,
                r.channel,
                r.path.display()
            )?;
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let mut rows = Vec::new();
    for (label, ch) in [("stable", Channel::Stable), ("preview", Channel::Preview)] {
        match install_composer(&paths, &ComposerRequest::Channel(ch)) {
            Ok(Installed { version, phar_path, already_present }) => rows.push(UpgradeRow {
                channel: label,
                version,
                path: phar_path,
                already_present,
            }),
            // Channel may legitimately be empty (preview often has no
            // open RC). Skip with no row.
            Err(_) if label == "preview" => {}
            Err(e) => return Err(e),
        }
    }
    let result = UpgradeResult { schema_version: 1, installed: rows };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}
