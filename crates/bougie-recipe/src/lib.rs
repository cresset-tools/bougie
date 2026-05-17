//! Recipe engine for `bougie make`. See RECIPES.md.
//!
//! Tasks are declared in `[task.<name>]` tables in `bougie.toml` (or
//! in a builtin recipe shipped with the binary). Each task is phony
//! unless it declares `creates`, in which case the recipe is freshness
//! gated on mtime against its file-path deps and the `creates` of its
//! named-task deps.
//!
//! Unix-only: the `freshness` and `run` modules use POSIX-specific APIs
//! (`utimensat` with `UTIME_NOW`/`UTIME_OMIT`, `/bin/sh`). On Windows
//! the crate compiles to an empty lib; the `bougie` bin already gates
//! its recipe dispatch behind `cfg(unix)` and routes Windows callers
//! to a clearer error.

#![cfg(unix)]

pub mod builtin;
pub mod dag;
pub mod freshness;
pub mod parser;
pub mod run;

pub use builtin::{detect_from_text, merge_with_builtin, BUILTINS};
pub use dag::{Dag, DagError};
pub use parser::{parse, Recipe, TaskDef};
pub use run::{run_task, RunOptions, TaskOutcome, TaskStatus};

use std::path::Path;
use std::sync::OnceLock;

/// Optional hook for injecting `BOUGIE_SERVICE_*` env vars into recipe
/// shell steps. Set once at process startup by the bougie binary (which
/// owns the IPC client to bougied); leaving it unset keeps the recipe
/// engine usable in isolation.
pub type ServiceEnvProvider = fn(&Path) -> Vec<(String, String)>;

static SERVICE_ENV: OnceLock<ServiceEnvProvider> = OnceLock::new();

pub fn set_service_env_provider(provider: ServiceEnvProvider) {
    let _ = SERVICE_ENV.set(provider);
}

pub(crate) fn service_env_provider() -> Option<ServiceEnvProvider> {
    SERVICE_ENV.get().copied()
}
