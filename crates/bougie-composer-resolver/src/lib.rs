//! Native Composer dependency resolver + installer for bougie.
//!
//! The crate ships the parallel dist downloader/extractor, the
//! pubgrub-based solver + metadata fetcher, the `install_from_lock`
//! orchestrator, and Phase D git support — resolving versions from a
//! `{type: vcs}` repository and installing from a git `source` (the
//! [`vcs`] plumbing). The original `RESOLVER_PLAN.md` design doc was
//! removed once the resolver shipped (it lives in git history); ongoing
//! test work is tracked in `RESOLVER_TEST_PLAN.md`.

pub mod audit;
pub mod hash;
pub mod install;
pub mod metadata;
pub mod package_name;
pub mod platform;
pub mod query;
pub mod update;
pub mod vcs;
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
    fetch_and_extract_dists, install_from_lock, install_from_lock_with_patches, DistOutcome,
    DistRequest, InstallOptions, InstallSummary, ScriptHooks,
};
pub use audit::{fetch_advisories, Advisory};
pub use platform::{PlatformEnv, PlatformIgnore};
pub use query::{
    funding, latest_versions, licenses, DependencyGraph, Edge, Node, RootNode, Section,
};
pub use update::{
    dry_run_update, dry_run_update_partial, resolve_for_lockfile, resolve_for_lockfile_partial,
    DryRunOptions, LockfileSolveOutcome, PartialUpdate, ResolutionStrategy, ResolvedPackage,
    UpdateSummary,
};
