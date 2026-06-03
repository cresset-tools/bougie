//! `bougie services daemon {status,stop,version}` — inspect and
//! control the `bougied` background daemon.
//!
//! All three subcommands are pure IPC calls. The client auto-spawns
//! `bougied` if it isn't already running (see `client::call`).

use super::client;
use bougie_cli::OutputFormat;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use eyre::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Write};
use std::process::ExitCode;

// -------------------- `daemon status` --------------------

#[derive(Debug, Serialize)]
pub struct StatusResult {
    pub schema_version: u32,
    pub socket: String,
    /// PID of the daemon's primary process. Read from
    /// `$BOUGIE_HOME/state/bougied.pid` (which the daemon stamps at
    /// startup; see `bougie_daemon::daemon::run`).
    pub pid: Option<u32>,
    pub services: Vec<ServiceRow>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ServiceRow {
    pub name: String,
    pub state: String,
}

#[derive(Debug, Deserialize)]
struct DaemonStatusReply {
    #[serde(default)]
    services: Vec<ServiceRow>,
}

impl Render for StatusResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "bougied socket: {}", self.socket)?;
        match self.pid {
            Some(pid) => writeln!(w, "bougied pid:    {pid}")?,
            None => writeln!(w, "bougied pid:    (not running)")?,
        }
        if self.services.is_empty() {
            writeln!(w, "managed services: 0")?;
        } else {
            writeln!(w, "managed services:")?;
            for s in &self.services {
                writeln!(w, "  {:20} {}", s.name, s.state)?;
            }
        }
        Ok(())
    }
}

pub fn status(format: OutputFormat) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let reply: DaemonStatusReply = client::call(&paths, "status", Value::Null)?;
    let pid = read_pid(&paths);
    let result = StatusResult {
        schema_version: 1,
        socket: paths.bougied_sock().display().to_string(),
        pid,
        services: reply.services,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn read_pid(paths: &Paths) -> Option<u32> {
    let s = std::fs::read_to_string(paths.bougied_pid()).ok()?;
    s.trim().parse().ok()
}

// -------------------- `daemon stop` --------------------

#[derive(Debug, Serialize)]
pub struct StopResult {
    pub schema_version: u32,
    pub ok: bool,
    /// Services the daemon drained during shutdown, in teardown order.
    /// Empty when nothing was running (or the daemon wasn't up).
    pub stopped: Vec<String>,
    /// `true` when there was no daemon to stop — the socket was already
    /// gone before we asked.
    pub already_stopped: bool,
}

#[derive(Debug, Deserialize)]
struct DaemonShutdownReply {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    stopped: Vec<String>,
}

impl Render for StopResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.already_stopped {
            return writeln!(w, "bougied: not running");
        }
        if !self.ok {
            return writeln!(w, "bougied: shutdown failed");
        }
        writeln!(w, "bougied: stopped")
    }
}

pub fn stop(format: OutputFormat) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    // If the socket isn't there, bougied isn't running — return OK
    // (idempotent stop is the user-friendly behavior).
    if !paths.bougied_sock().exists() {
        let result = StopResult {
            schema_version: 1,
            ok: true,
            stopped: Vec::new(),
            already_stopped: true,
        };
        emit(format, &result)?;
        return Ok(ExitCode::SUCCESS);
    }
    // `daemon.shutdown` streams a `progress` frame per service as it
    // drains; `client::call` forwards those to stderr, so the user sees
    // live teardown progress before the terminal reply arrives here.
    let reply: DaemonShutdownReply = client::call(&paths, "daemon.shutdown", Value::Null)?;
    // The terminal frame means the drain is done. Now wait for the
    // process to exit and unlink its socket, so `stop` only returns once
    // bougied is fully gone rather than merely signalled.
    if reply.ok {
        client::wait_for_shutdown(&paths)?;
    }
    let result = StopResult {
        schema_version: 1,
        ok: reply.ok,
        stopped: reply.stopped,
        already_stopped: false,
    };
    emit(format, &result)?;
    if reply.ok {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

// -------------------- `daemon version` --------------------

#[derive(Debug, Serialize)]
pub struct VersionResult {
    pub schema_version: u32,
    pub daemon: DaemonVersion,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DaemonVersion {
    pub version: String,
    #[serde(default)]
    pub build_hash: String,
}

impl Render for VersionResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "bougied {}", self.daemon.version)?;
        if !self.daemon.build_hash.is_empty() {
            writeln!(w, "build:   {}", self.daemon.build_hash)?;
        }
        Ok(())
    }
}

pub fn version(format: OutputFormat) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let daemon: DaemonVersion = client::call(&paths, "daemon.version", Value::Null)?;
    let result = VersionResult { schema_version: 1, daemon };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}
