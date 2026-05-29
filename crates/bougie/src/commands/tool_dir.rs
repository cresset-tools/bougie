//! `bougie tool dir [<package>]` — print the tools root, or a specific
//! tool's install directory.

use bougie_cli::OutputFormat;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use bougie_tool::request;
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ToolDirResult {
    pub schema_version: u32,
    pub path: PathBuf,
}

impl Render for ToolDirResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "{}", self.path.display())
    }
}

pub fn run(format: OutputFormat, package: Option<String>) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let path = match package {
        Some(p) => {
            let req = request::parse(&p)?;
            paths.tool_dir(&req.package())
        }
        None => paths.tools(),
    };
    emit(format, &ToolDirResult { schema_version: 1, path })?;
    Ok(ExitCode::SUCCESS)
}
