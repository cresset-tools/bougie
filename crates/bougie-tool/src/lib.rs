//! `bougie tool` — globally-installed, isolated PHP CLI tools.
//!
//! A *tool* is a Composer-installable package whose `bin` entries are
//! exposed globally. Each tool gets its own `composer.json` + `vendor/`
//! tree under `$BOUGIE_LOCAL/tools/<vendor>-<name>/`, independent of
//! any project. Users think "I installed phpstan"; the per-tool vendor
//! dir, the receipt file, and the PATH shim are how that illusion
//! holds up.
//!
//! See `TOOL_PLAN.md` at the repo root for the full design. Phase 1
//! covers Unix-only persistent install / uninstall / list / dir plus
//! the `tool-exec` runtime shim.

pub mod classify;
pub mod exec;
pub mod inject;
pub mod install;
pub mod upgrade;
pub mod list;
pub mod receipt;
pub mod request;
pub mod resolve;
pub mod uninstall;
pub mod wrapper;
