//! `bougie server` — foreground HTTP server with per-request xdebug
//! routing. See SERVER.md in php-build-standalone for the spec.
//!
//! Phase 0 ships only the config plumbing and the `add`/`remove`/`list`
//! helpers. `run` is a placeholder that errors out until phase 1 adds
//! the listener.

pub mod config;
pub mod helpers;
pub mod hosts;
pub mod run;
pub mod tls;
