//! Anonymous, opt-in usage telemetry (see `TELEMETRY_PLAN.md`).
//!
//! Everything here is fail-soft by contract: telemetry must never fail,
//! slow, or noisy-up a command. Fallible paths swallow their errors
//! (surfacing them only via `tracing::debug!`), and nothing in this
//! crate performs network I/O during a user command — events are
//! appended to an on-disk spool and uploaded later by a detached,
//! deprioritized flush subprocess (mode `on` only).
//!
//! Consent is a tri-state mode — `off` / `local` / `on` — resolved from
//! `DO_NOT_TRACK`, `BOUGIE_TELEMETRY`, and the mode file under the
//! global config dir, in that order. `local` spools but never uploads;
//! `bougie telemetry log` shows exactly what would be sent.

pub mod clock;
pub mod event;
pub mod ids;
pub mod mode;
pub mod recorder;
pub mod spool;

pub use event::{outcome_for_error, OUTCOME_OK};
pub use mode::{Mode, ModeState, Source, CONSENT_VERSION};
pub use recorder::{BinInfo, Recorder};
pub use spool::Spool;
