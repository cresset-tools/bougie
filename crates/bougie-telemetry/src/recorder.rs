//! Per-invocation recorder: the one type the `bougie` bin talks to.
//!
//! `Recorder::init` is infallible and cheap when telemetry is off (two
//! env reads + one small file read, no allocation beyond the state).
//! When the mode is `local` or `on`, `record_command` serializes one
//! event line into the spool. Uploads never happen here.

use crate::clock::UtcHour;
use crate::event::{self, CommandEvent, Common, SCHEMA};
use crate::ids;
use crate::mode::{self, Mode};
use crate::spool::Spool;
use std::path::PathBuf;
use std::time::Duration;

/// Version identity the bin passes in (`env!("CARGO_PKG_VERSION")` +
/// `bougie_cli::BUILD_SHA`); the crate never depends on bougie-cli.
#[derive(Debug, Clone, Copy)]
pub struct BinInfo {
    pub version: &'static str,
    pub build_sha: Option<&'static str>,
}

#[derive(Debug)]
pub struct Recorder {
    inner: Option<Inner>,
}

#[derive(Debug)]
struct Inner {
    mode: Mode,
    spool: Spool,
    command: &'static str,
    info: BinInfo,
    install_id: String,
    invocation: String,
    ci: bool,
    install_method: &'static str,
}

impl Recorder {
    /// A recorder that records nothing — for the telemetry-management
    /// commands themselves (`telemetry reset` re-spooling its own event
    /// after the purge would be absurd) and other internal invocations.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Resolve mode and set up the spool. Never fails: any problem
    /// (unresolvable paths, unreadable files) degrades to disabled.
    pub fn init(command: &'static str, info: BinInfo) -> Self {
        let config_dir = bougie_paths::config_dir().ok();
        let mode_file = config_dir.as_ref().map(|d| d.join("telemetry"));
        let state = mode::resolve_from_env(mode_file.as_deref());
        if state.mode == Mode::Off {
            return Self { inner: None };
        }
        let Ok(paths) = bougie_paths::Paths::from_env() else {
            return Self { inner: None };
        };
        let install_id = match (state.mode, config_dir.as_deref()) {
            // `on` is consent — mint lazily if the file is missing
            // (covers BOUGIE_TELEMETRY=on with no prior `telemetry on`).
            (Mode::On, Some(dir)) => {
                ids::read_or_mint(dir).unwrap_or_else(|| ids::UNSET.to_owned())
            }
            // `local` deliberately mints nothing persistent.
            (_, Some(dir)) => ids::read(dir).unwrap_or_else(|| ids::UNSET.to_owned()),
            (_, None) => ids::UNSET.to_owned(),
        };
        Self {
            inner: Some(Inner {
                mode: state.mode,
                spool: Spool::new(paths.cache()),
                command,
                info,
                install_id,
                invocation: ids::invocation_id(),
                ci: mode::is_ci(),
                install_method: install_method(config_dir, info),
            }),
        }
    }

    /// Effective mode (`Off` when disabled).
    pub fn mode(&self) -> Mode {
        self.inner.as_ref().map_or(Mode::Off, |i| i.mode)
    }

    /// Spool one `command` event. Failures are swallowed: telemetry
    /// must never fail a command (the §9.2 event-sink contract).
    pub fn record_command(&self, duration: Duration, outcome: &'static str, exit_code: u8) {
        let Some(inner) = &self.inner else { return };
        let now = UtcHour::now();
        let event = CommandEvent {
            common: Common {
                schema: SCHEMA,
                event: "command",
                ts: now.rfc3339(),
                install_id: inner.install_id.clone(),
                invocation: inner.invocation.clone(),
                bougie_version: inner.info.version,
                build_sha: inner.info.build_sha,
                os: event::os(),
                arch: event::arch(),
                libc: event::libc(),
                ci: inner.ci,
                install_method: inner.install_method,
            },
            name: inner.command,
            duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            outcome,
            exit_code,
        };
        match serde_json::to_string(&event) {
            Ok(line) => inner.spool.append(&now.date(), &line),
            Err(err) => tracing::debug!("telemetry event serialization failed: {err}"),
        }
    }
}

/// Best-effort install-channel detection: the dist install receipt
/// marks installer-managed binaries; a missing build SHA marks a
/// crates.io tarball build (`cargo install`).
fn install_method(config_dir: Option<PathBuf>, info: BinInfo) -> &'static str {
    let receipt = config_dir
        .map(|d| d.join("bougie-receipt.json"))
        .is_some_and(|p| p.exists());
    if receipt {
        "installer"
    } else if info.build_sha.is_none() {
        "cargo"
    } else {
        "unknown"
    }
}
