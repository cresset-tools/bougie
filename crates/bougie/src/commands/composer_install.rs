use bougie_cli::OutputFormat;
use bougie_composer::{default_request, install_composer, parse_request, Installed};
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct InstallResult {
    pub schema_version: u32,
    pub version: String,
    pub path: PathBuf,
    pub already_present: bool,
}

impl Render for InstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let verb = if self.already_present { "already" } else { "installed" };
        writeln!(
            w,
            "{verb} composer {} at {}",
            self.version,
            self.path.display()
        )
    }
}

pub fn run(
    format: OutputFormat,
        request_str: Option<&str>,
) -> Result<ExitCode> {
    let request = match request_str {
        Some(s) => parse_request(s)?,
        None => default_request(),
    };
    let paths = Paths::from_env()?;
    let installed: Installed = install_composer(&paths, &request)?;
    let result = InstallResult {
        schema_version: 1,
        version: installed.version,
        path: installed.phar_path,
        already_present: installed.already_present,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}
