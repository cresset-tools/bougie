use crate::cli::OutputFormat;
use crate::output::{emit, Render};
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Phase 4 wires the real fingerprint; phase 2 ships a placeholder so
/// the JSON schema stays stable.
const TRUST_ROOT_PLACEHOLDER: &str = "<not yet wired>";

#[derive(Debug, Serialize)]
pub struct VersionResult {
    pub schema_version: u32,
    pub bougie: VersionInfo,
}

#[derive(Debug, Serialize)]
pub struct VersionInfo {
    pub version: &'static str,
    pub trust_root_fingerprint: &'static str,
}

impl Render for VersionResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "bougie {}", self.bougie.version)?;
        writeln!(w, "trust-root: {}", self.bougie.trust_root_fingerprint)
    }
}

pub fn run(format: OutputFormat, field: Option<&str>, short: bool) -> Result<ExitCode> {
    let result = VersionResult {
        schema_version: 1,
        bougie: VersionInfo {
            version: VERSION,
            trust_root_fingerprint: TRUST_ROOT_PLACEHOLDER,
        },
    };
    if short {
        emit(OutputFormat::Text, Some("bougie.version"), &result)?;
    } else {
        emit(format, field, &result)?;
    }
    Ok(ExitCode::SUCCESS)
}
