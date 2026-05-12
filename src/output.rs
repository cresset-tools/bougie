//! Output discipline per CLI.md §9: `--format text|json-v1`, `--field`,
//! and the NDJSON event stream on stderr.

use crate::cli::OutputFormat;
use eyre::{eyre, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::sync::OnceLock;

/// Process-wide flag: should long-running fetches render an
/// `indicatif` progress bar to stderr? Set once at `bougie::run`
/// entry and read by `fetch::fetch_to_partial`. Hidden when stderr
/// isn't a TTY, when `--quiet` is set, or when `--format json-v1`
/// is requested (a TTY progress bar would corrupt the NDJSON event
/// stream callers are likely parsing).
static PROGRESS_VISIBLE: OnceLock<bool> = OnceLock::new();

pub fn set_progress_visible(visible: bool) {
    let _ = PROGRESS_VISIBLE.set(visible);
}

pub fn progress_visible() -> bool {
    *PROGRESS_VISIBLE.get().unwrap_or(&false)
}

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
///
/// Stdout is wrapped in [`anstream::AutoStream`] so commands that emit
/// ANSI escape codes get them stripped automatically when stdout is
/// not a terminal (or when `NO_COLOR` is set).
pub fn emit<R: Render>(format: OutputFormat, field: Option<&str>, result: &R) -> Result<()> {
    let stdout = io::stdout();
    let mut w = anstream::AutoStream::auto(stdout.lock());
    write_result(&mut w, format, field, result)
}

/// Like `emit`, but pipes the rendered output through `$PAGER` (default
/// `less`) when stdout is a terminal and the user isn't extracting a
/// field or asking for JSON. Falls back to direct stdout if the pager
/// can't be spawned.
pub fn emit_paged<R: Render>(format: OutputFormat, field: Option<&str>, result: &R) -> Result<()> {
    use std::io::IsTerminal;

    let want_pager = field.is_none()
        && matches!(format, OutputFormat::Text)
        && io::stdout().is_terminal();
    if !want_pager {
        return emit(format, field, result);
    }

    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".into());
    let pager = pager.trim();
    if pager.is_empty() || pager == "cat" {
        return emit(format, field, result);
    }

    let mut parts = pager.split_whitespace();
    let Some(cmd) = parts.next() else {
        return emit(format, field, result);
    };
    let args: Vec<&str> = parts.collect();

    let mut child_cmd = std::process::Command::new(cmd);
    child_cmd
        .args(&args)
        .stdin(std::process::Stdio::piped());
    // Match git's defaults: quit-if-one-screen, raw-control-chars,
    // no-init. Only set LESS if the user hasn't already.
    if std::env::var_os("LESS").is_none() {
        child_cmd.env("LESS", "FRX");
    }
    let mut child = match child_cmd.spawn() {
        Ok(c) => c,
        Err(_) => return emit(format, field, result),
    };
    {
        let stdin = child.stdin.take().expect("piped stdin");
        // We only entered this branch because stdout is a terminal,
        // so pass ANSI through to the pager unless the user opted out
        // via `NO_COLOR`. `less -R` (set via LESS=FRX) renders the codes.
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let res = if no_color {
            let boxed: Box<dyn Write + Send> = Box::new(stdin);
            let mut w = anstream::StripStream::new(boxed);
            write_result(&mut w, format, field, result)
        } else {
            let mut w = stdin;
            write_result(&mut w, format, field, result)
        };
        // Ignore broken-pipe errors: the user may have quit the pager.
        if let Err(e) = res
            && e.downcast_ref::<io::Error>()
                .is_none_or(|ioe| ioe.kind() != io::ErrorKind::BrokenPipe)
        {
            let _ = child.wait();
            return Err(e);
        }
    }
    let _ = child.wait();
    Ok(())
}

fn write_result<W: Write, R: Render>(
    w: &mut W,
    format: OutputFormat,
    field: Option<&str>,
    result: &R,
) -> Result<()> {
    if let Some(path) = field {
        return write_field(w, result, path);
    }
    match format {
        OutputFormat::Text => result.render_text(w)?,
        OutputFormat::JsonV1 => {
            serde_json::to_writer(&mut *w, result)?;
            writeln!(w)?;
        }
    }
    Ok(())
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
