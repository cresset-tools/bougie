//! `bougie server` — foreground HTTP server with per-request xdebug
//! routing. See SERVER.md in php-build-standalone for the spec.
//!
//! Phase 0 ships only the config plumbing and the `add`/`remove`/`list`
//! helpers. `run` is a placeholder that errors out until phase 1 adds
//! the listener.

pub mod autoloader_manager;
pub mod config;
pub mod conf_d;
pub mod control;
pub mod fastcgi;
pub mod helpers;
/// `bougie server hosts apply` — manages bougie's sentinel block in
/// `/etc/hosts`. Unix-only: Windows uses `%WINDIR%\System32\drivers\etc\hosts`
/// with different permission semantics and a different newline
/// convention; that's a phase-2+ effort.
#[cfg(unix)]
pub mod hosts;
pub mod log;
pub mod paths;
pub mod pool;
pub mod router;
pub mod run;
pub mod static_files;
pub mod watch_registry;
/// `bougie server tls install` — installs a dev CA into the system
/// trust store. Unix-only: macOS uses `security`, Linux uses
/// `update-ca-trust`/`update-ca-certificates`. Windows certmgr work
/// is a phase-2+ effort.
#[cfg(unix)]
pub mod tls;
pub mod watcher;
pub mod xdebug;
