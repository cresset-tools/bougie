//! Update lane: spools a best-effort `update` event when `self update`
//! successfully swaps the on-disk binary for a newer release.
//!
//! Like the crash lane, this is a *second* event spooled directly at the
//! point the interesting thing happened, rather than through the
//! [`Recorder`](crate::Recorder) (which owns the invocation's `command`
//! event). Consent is resolved here so an `off` / `local` / `on` decision
//! is honored, and every step is best-effort — failing to record must
//! never fail the update the user just performed.

use crate::clock::UtcHour;
use crate::event::{self, Common, SCHEMA, UpdateEvent};
use crate::ids;
use crate::mode::{self, Mode};
use crate::recorder::{BinInfo, install_method};
use crate::spool::Spool;

/// Spool one `update` event for a completed binary swap. `from` is the
/// release that was replaced, `to` the release now on disk. Call this
/// *only* after the swap actually happened — never on the "already up to
/// date" / "assets not published yet" no-op paths, which perform no
/// update. Best-effort: consent `off`, unresolvable paths, or a full
/// spool simply record nothing.
pub fn record(info: BinInfo, from: &str, to: &str) {
    let mode_file = bougie_paths::telemetry_mode_file().ok();
    if mode::resolve_from_env(mode_file.as_deref()).mode == Mode::Off {
        return;
    }
    let Ok(paths) = bougie_paths::Paths::from_env() else {
        return;
    };

    // Read the id the invocation's `command` event already minted (mode
    // `on` mints at `Recorder::init`, before dispatch); `local` mints
    // nothing, so this falls back to `unset` exactly like that event.
    let config_dir = bougie_paths::config_dir().ok();
    let install_id = config_dir
        .as_deref()
        .and_then(ids::read)
        .unwrap_or_else(|| ids::UNSET.to_owned());

    let now = UtcHour::now();
    let ev = UpdateEvent {
        common: Common {
            schema: SCHEMA,
            event: "update",
            ts: now.rfc3339(),
            install_id,
            invocation: ids::invocation_id(),
            bougie_version: info.version,
            build_sha: info.build_sha,
            os: event::os(),
            arch: event::arch(),
            libc: event::libc(),
            ci: mode::is_ci(),
            install_method: install_method(config_dir, info),
        },
        from_version: from.to_owned(),
        to_version: to.to_owned(),
    };
    match serde_json::to_string(&ev) {
        Ok(line) => Spool::new(paths.cache()).append(&now.date(), &line),
        Err(err) => tracing::debug!("telemetry update event serialization failed: {err}"),
    }
}
