//! `bougie ext add` / `bougie ext remove` delegate to Composer per
//! CLI.md §3.2.1 / §3.2.2 — bougie does not edit composer.json
//! directly. The implicit `bougie sync` step happens after the composer
//! call; for v0.1 it is left to the user.

use crate::cli::OutputFormat;
use crate::output::{emit, Render};
use eyre::{eyre, Result, WrapErr};
use serde::Serialize;
use std::io::{self, Write};
use std::process::{Command, ExitCode};

#[derive(Debug, Serialize)]
pub struct ExtAddRemoveResult {
    pub schema_version: u32,
    pub action: &'static str,
    pub names: Vec<String>,
}

impl Render for ExtAddRemoveResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for n in &self.names {
            writeln!(w, "{} ext-{n}", self.action)?;
        }
        writeln!(w, "next: run `bougie sync` to apply")
    }
}

pub fn add(format: OutputFormat, field: Option<&str>, names: Vec<String>) -> Result<ExitCode> {
    delegate("require", "add", names, format, field)
}

pub fn remove(format: OutputFormat, field: Option<&str>, names: Vec<String>) -> Result<ExitCode> {
    delegate("remove", "remove", names, format, field)
}

fn delegate(
    composer_verb: &str,
    action: &'static str,
    names: Vec<String>,
    format: OutputFormat,
    field: Option<&str>,
) -> Result<ExitCode> {
    if names.is_empty() {
        return Err(eyre!("no extensions specified"));
    }
    if which::which("composer").is_err() {
        return Err(eyre!(
            "composer is not on PATH; install it first (composer is the source of truth for require.ext-*)"
        ));
    }
    let mut cmd = Command::new("composer");
    cmd.arg(composer_verb);
    for n in &names {
        cmd.arg(format!("ext-{n}"));
    }
    let status = cmd
        .status()
        .wrap_err("invoking composer")?;
    if !status.success() {
        return Err(eyre!("composer exited with status {status}"));
    }
    let result = ExtAddRemoveResult { schema_version: 1, action, names };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}
