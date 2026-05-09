//! Output discipline per CLI.md §9: `--format text|json-v1`, `--field`,
//! and the NDJSON event stream on stderr.

use crate::cli::OutputFormat;
use eyre::{eyre, Result};
use serde::Serialize;
use std::io::{self, Write};

/// Implemented by every command's Result struct.
///
/// JSON serialization comes from `serde::Serialize` directly; the text
/// rendering is a per-command concern (versions need different shapes
/// from paths from listings).
pub trait Render: Serialize {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()>;
}

/// Emit a command's final result to stdout. Honors `--field`, falling
/// back to format-driven rendering.
pub fn emit<R: Render>(format: OutputFormat, field: Option<&str>, result: &R) -> Result<()> {
    let stdout = io::stdout();
    let mut w = stdout.lock();
    if let Some(path) = field {
        write_field(&mut w, result, path)
    } else {
        match format {
            OutputFormat::Text => {
                result.render_text(&mut w)?;
            }
            OutputFormat::JsonV1 => {
                serde_json::to_writer(&mut w, result)?;
                writeln!(w)?;
            }
        }
        Ok(())
    }
}

fn write_field<W: Write, R: Serialize>(w: &mut W, value: &R, path: &str) -> Result<()> {
    let v = serde_json::to_value(value)?;
    let mut cur = &v;
    for seg in path.split('.') {
        cur = cur
            .get(seg)
            .ok_or_else(|| eyre!("field not found: {path}"))?;
    }
    match cur {
        serde_json::Value::String(s) => writeln!(w, "{s}")?,
        serde_json::Value::Number(n) => writeln!(w, "{n}")?,
        serde_json::Value::Bool(b) => writeln!(w, "{b}")?,
        serde_json::Value::Null => writeln!(w)?,
        _ => return Err(eyre!("field is not scalar: {path}")),
    }
    Ok(())
}

/// One line in the §9.2 NDJSON event stream.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Event<'a> {
    Phase { name: &'a str },
    Fetch { url: &'a str, bytes: Option<u64> },
    Cache { kind: &'a str, hit: bool },
    Warning { message: &'a str },
}

#[derive(Serialize)]
struct EventEnvelope<'a> {
    schema_version: u32,
    #[serde(flatten)]
    event: &'a Event<'a>,
}

/// Long-running commands emit phase / fetch / cache / warning events
/// here. Render mode follows the global `--format`.
#[derive(Debug, Clone, Copy)]
pub struct EventSink {
    format: OutputFormat,
    quiet: bool,
}

impl EventSink {
    pub fn new(format: OutputFormat, quiet: bool) -> Self {
        Self { format, quiet }
    }

    /// Emit one event to stderr. Failures are swallowed: telemetry
    /// must never fail a command.
    pub fn emit(&self, event: &Event<'_>) {
        if self.quiet {
            return;
        }
        let stderr = io::stderr();
        let mut w = stderr.lock();
        let _ = match self.format {
            OutputFormat::Text => write_event_text(&mut w, event),
            OutputFormat::JsonV1 => write_event_json(&mut w, event),
        };
    }
}

fn write_event_text<W: Write>(w: &mut W, event: &Event<'_>) -> io::Result<()> {
    match event {
        Event::Phase { name } => writeln!(w, "{name}…"),
        Event::Fetch { url, bytes } => match bytes {
            Some(b) => writeln!(w, "fetch {url} ({b} bytes)"),
            None => writeln!(w, "fetch {url}"),
        },
        Event::Cache { kind, hit } => {
            writeln!(w, "cache {kind} {}", if *hit { "hit" } else { "miss" })
        }
        Event::Warning { message } => writeln!(w, "warning: {message}"),
    }
}

fn write_event_json<W: Write>(w: &mut W, event: &Event<'_>) -> io::Result<()> {
    let envelope = EventEnvelope { schema_version: 1, event };
    serde_json::to_writer(&mut *w, &envelope)?;
    writeln!(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Sample {
        schema_version: u32,
        bougie: Inner,
    }

    #[derive(Serialize)]
    struct Inner {
        version: String,
        active: bool,
    }

    impl Render for Sample {
        fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
            writeln!(w, "bougie {}", self.bougie.version)
        }
    }

    #[test]
    fn write_field_extracts_string() {
        let s = Sample {
            schema_version: 1,
            bougie: Inner { version: "0.1.0".into(), active: true },
        };
        let mut buf = Vec::new();
        write_field(&mut buf, &s, "bougie.version").unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "0.1.0\n");
    }

    #[test]
    fn write_field_extracts_bool() {
        let s = Sample {
            schema_version: 1,
            bougie: Inner { version: "0.1.0".into(), active: true },
        };
        let mut buf = Vec::new();
        write_field(&mut buf, &s, "bougie.active").unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "true\n");
    }

    #[test]
    fn write_field_missing_errors() {
        let s = Sample {
            schema_version: 1,
            bougie: Inner { version: "0.1.0".into(), active: true },
        };
        let mut buf = Vec::new();
        let err = write_field(&mut buf, &s, "bougie.nonsense").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn write_field_non_scalar_errors() {
        let s = Sample {
            schema_version: 1,
            bougie: Inner { version: "0.1.0".into(), active: true },
        };
        let mut buf = Vec::new();
        let err = write_field(&mut buf, &s, "bougie").unwrap_err();
        assert!(err.to_string().contains("not scalar"));
    }

    #[test]
    fn event_json_envelope_carries_schema_version() {
        let event = Event::Phase { name: "Resolving" };
        let mut buf = Vec::new();
        write_event_json(&mut buf, &event).unwrap();
        let line = String::from_utf8(buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["type"], "phase");
        assert_eq!(parsed["name"], "Resolving");
    }

    #[test]
    fn event_text_phase_has_ellipsis() {
        let event = Event::Phase { name: "Resolving" };
        let mut buf = Vec::new();
        write_event_text(&mut buf, &event).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "Resolving…\n");
    }
}
