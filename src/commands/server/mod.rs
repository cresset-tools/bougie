//! `bougie server` — foreground HTTP server with per-request xdebug
//! routing. See SERVER.md in php-build-standalone for the spec.
//!
//! Phase 0 ships only the config plumbing and the `add`/`remove`/`list`
//! helpers. `run` is a placeholder that errors out until phase 1 adds
//! the listener.

pub mod config;
pub mod conf_d;
pub mod fastcgi;
pub mod helpers;
pub mod hosts;
pub mod log;
pub mod paths;
pub mod pool;
pub mod router;
pub mod run;
pub mod static_files;
pub mod tls;
pub mod xdebug;
