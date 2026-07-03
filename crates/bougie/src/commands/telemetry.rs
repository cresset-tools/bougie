//! `bougie telemetry` — inspect and set the anonymous-telemetry
//! consent mode (`status` / `on` / `off` / `local` / `log` / `reset`).
//!
//! The mode file lives in the global config dir (next to the dist
//! install receipt) so the installer consent snippets can write it
//! from plain shell; see `bougie_paths::telemetry_mode_file`.

use bougie_cli::{OutputFormat, TelemetryCommand};
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use bougie_telemetry::clock::UtcHour;
use bougie_telemetry::spool::Spool;
use bougie_telemetry::{ids, mode, Mode};
use eyre::{Result, WrapErr};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

pub fn run(format: OutputFormat, command: Option<TelemetryCommand>) -> Result<ExitCode> {
    match command.unwrap_or(TelemetryCommand::Status) {
        TelemetryCommand::Status => status(format),
        TelemetryCommand::On => set_mode(format, Mode::On),
        TelemetryCommand::Off => set_mode(format, Mode::Off),
        TelemetryCommand::Local => set_mode(format, Mode::Local),
        TelemetryCommand::Log { lines } => log(format, lines),
        TelemetryCommand::Reset => reset(format),
    }
}

#[derive(Debug, Serialize)]
struct StatusResult {
    schema_version: u32,
    mode: &'static str,
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    consent_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    consent_version: Option<u32>,
    mode_file: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    install_id: Option<String>,
    spooled_events: usize,
    spool_bytes: u64,
    spool_dir: PathBuf,
}

impl Render for StatusResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "mode:       {} (from {})", self.mode, self.source)?;
        if let (Some(date), Some(version)) = (&self.consent_date, self.consent_version) {
            writeln!(w, "consented:  {date} (consent v{version})")?;
        }
        writeln!(w, "mode file:  {}", self.mode_file.display())?;
        match &self.install_id {
            Some(id) => writeln!(w, "install id: {id}")?,
            None => writeln!(w, "install id: none (minted by `bougie telemetry on`)")?,
        }
        writeln!(
            w,
            "spool:      {} event(s), {} bytes — {}",
            self.spooled_events,
            self.spool_bytes,
            self.spool_dir.display()
        )?;
        if self.mode == "off" && self.source == "unset" {
            writeln!(w, "hint:       enable with `bougie telemetry on` (details: TELEMETRY.md)")?;
        }
        Ok(())
    }
}

fn status(format: OutputFormat) -> Result<ExitCode> {
    let config_dir = bougie_paths::config_dir()?;
    let mode_file = bougie_paths::telemetry_mode_file()?;
    let state = mode::resolve_from_env(Some(&mode_file));
    let paths = Paths::from_env()?;
    let spool = Spool::new(paths.cache());
    let result = StatusResult {
        schema_version: 1,
        mode: state.mode.as_str(),
        source: state.source.as_str(),
        consent_date: state.consent_date.clone(),
        consent_version: state.consent_version,
        mode_file,
        install_id: ids::read(&config_dir),
        spooled_events: spool.event_count(),
        spool_bytes: spool.total_bytes(),
        spool_dir: spool.dir().to_path_buf(),
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Serialize)]
struct SetResult {
    schema_version: u32,
    mode: &'static str,
    mode_file: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    install_id: Option<String>,
}

impl Render for SetResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        match self.mode {
            "on" => {
                writeln!(w, "telemetry is on — anonymous usage events upload in batches.")?;
                writeln!(w, "  inspect anytime:  bougie telemetry log")?;
                writeln!(w, "  full field list:  https://bougie.tools/telemetry")?;
            }
            "local" => {
                writeln!(
                    w,
                    "telemetry is local-only: events are recorded to the spool but never uploaded."
                )?;
            }
            _ => {
                writeln!(w, "telemetry is off. Enable later with: bougie telemetry on")?;
            }
        }
        Ok(())
    }
}

fn set_mode(format: OutputFormat, mode: Mode) -> Result<ExitCode> {
    let config_dir = bougie_paths::config_dir()?;
    let path = bougie_paths::telemetry_mode_file()?;
    let date = UtcHour::now().date();
    mode::write_file(&path, mode, &date)
        .wrap_err_with(|| format!("writing {}", path.display()))?;
    let install_id = match mode {
        // Turning on is the consent moment — mint the anonymous id.
        Mode::On => ids::read_or_mint(&config_dir),
        _ => ids::read(&config_dir),
    };
    let result =
        SetResult { schema_version: 1, mode: mode.as_str(), mode_file: path, install_id };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Serialize)]
struct LogResult {
    schema_version: u32,
    events: Vec<serde_json::Value>,
    #[serde(skip)]
    raw: Vec<String>,
}

impl Render for LogResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.raw.is_empty() {
            writeln!(w, "no spooled events")?;
        }
        for line in &self.raw {
            writeln!(w, "{line}")?;
        }
        Ok(())
    }
}

fn log(format: OutputFormat, lines: usize) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let spool = Spool::new(paths.cache());
    let raw = spool.last_lines(lines);
    let events = raw
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let result = LogResult { schema_version: 1, events, raw };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Serialize)]
struct ResetResult {
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    install_id: Option<String>,
    spool_dir: PathBuf,
}

impl Render for ResetResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "spool purged: {}", self.spool_dir.display())?;
        match &self.install_id {
            Some(id) => writeln!(w, "install id rotated: {id}")?,
            None => writeln!(w, "install id removed")?,
        }
        Ok(())
    }
}

fn reset(format: OutputFormat) -> Result<ExitCode> {
    let config_dir = bougie_paths::config_dir()?;
    let mode_file = bougie_paths::telemetry_mode_file()?;
    let paths = Paths::from_env()?;
    let spool = Spool::new(paths.cache());
    spool.purge();
    ids::remove(&config_dir);
    // Only re-mint when the *recorded* consent is `on`; an env override
    // is transient and shouldn't create persistent state on reset.
    let state = mode::resolve(None, None, mode::read_file(&mode_file).as_deref());
    let install_id = match state.mode {
        Mode::On => ids::mint(&config_dir),
        _ => None,
    };
    let result =
        ResetResult { schema_version: 1, install_id, spool_dir: spool.dir().to_path_buf() };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}
