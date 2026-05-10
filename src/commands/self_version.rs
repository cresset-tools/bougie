use crate::cli::OutputFormat;
use crate::index::describe_trust;
use crate::output::{Render, emit};
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Serialize)]
pub struct VersionResult {
    pub schema_version: u32,
    pub bougie: VersionInfo,
}

#[derive(Debug, Serialize)]
pub struct VersionInfo {
    pub version: &'static str,
    pub trust: TrustInfo,
}

#[derive(Debug, Serialize)]
pub struct TrustInfo {
    pub kind: &'static str,
    pub detail: String,
}

impl Render for VersionResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "bougie {}", self.bougie.version)?;
        writeln!(
            w,
            "trust ({}): {}",
            self.bougie.trust.kind, self.bougie.trust.detail
        )
    }
}

pub fn run(format: OutputFormat, field: Option<&str>, short: bool) -> Result<ExitCode> {
    let trust = describe_trust();
    let result = VersionResult {
        schema_version: 1,
        bougie: VersionInfo {
            version: VERSION,
            trust: TrustInfo { kind: trust.kind, detail: trust.detail },
        },
    };
    if short {
        emit(OutputFormat::Text, Some("bougie.version"), &result)?;
    } else {
        emit(format, field, &result)?;
    }
    Ok(ExitCode::SUCCESS)
}
