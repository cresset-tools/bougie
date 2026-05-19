//! Native Composer dependency resolver + installer for bougie.
//!
//! See `RESOLVER_PLAN.md` at the repo root for the full design. This
//! crate currently ships only the Phase A install primitive — a
//! parallel dist downloader + extractor that turns a list of
//! [`install::DistRequest`]s into a populated `vendor/<vendor>/<pkg>/`
//! tree. The pubgrub-based solver, metadata fetcher, and lockfile
//! reader land in later phases.

pub mod install;
pub mod verify;

pub use install::{
    fetch_and_extract_dists, install_from_lock, DistOutcome, DistRequest, InstallOptions,
    InstallSummary,
};
