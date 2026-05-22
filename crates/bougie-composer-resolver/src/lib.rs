//! Native Composer dependency resolver + installer for bougie.
//!
//! See `RESOLVER_PLAN.md` at the repo root for the full design. This
//! crate currently ships only the Phase A install primitive — a
//! parallel dist downloader + extractor that turns a list of
//! [`install::DistRequest`]s into a populated `vendor/<vendor>/<pkg>/`
//! tree. The pubgrub-based solver, metadata fetcher, and lockfile
//! reader land in later phases.

pub mod hash;
pub mod install;
pub mod metadata;
pub mod package_name;
pub mod update;
pub mod verify;

/// Saturating conversion of an elapsed time to a `u64` of milliseconds
/// for tracing fields. The underlying `Duration::as_millis()` returns
/// `u128` which `tracing` won't accept directly; truncation here is
/// only theoretically reachable past ~584 million years of elapsed
/// time, so `u64::MAX` is the right ceiling.
#[inline]
fn elapsed_ms(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

pub use install::{
    fetch_and_extract_dists, install_from_lock, DistOutcome, DistRequest, InstallOptions,
    InstallSummary,
};
pub use update::{
    dry_run_update, resolve_for_lockfile, DryRunOptions, LockfileSolveOutcome, ResolvedPackage,
    UpdateSummary,
};
