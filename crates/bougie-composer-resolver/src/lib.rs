//! Native Composer dependency resolver for bougie.
//!
//! The crate ships the pubgrub-based solver + metadata fetcher, Phase D
//! git support for resolving versions from a `{type: vcs}` repository (the
//! [`vcs`] plumbing), and [`install`], which wires bougie's fetcher, cache,
//! credentials and progress bar onto `composer-install`, the shared
//! install-from-lock crate. The original `RESOLVER_PLAN.md` design doc was
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

pub use audit::{Advisory, fetch_advisories};
pub use install::{
    DistArchive, DistRequest, InstallOptions, InstallSummary, LinkMode, ScriptHooks,
    fetch_and_extract_dists, install_from_lock, install_from_lock_with_patches,
};
pub use platform::{PlatformEnv, PlatformIgnore};
pub use query::{
    DependencyGraph, Edge, Node, RootNode, Section, funding, latest_versions, licenses,
};
pub use update::{
    DryRunOptions, LockfileSolveOutcome, PartialUpdate, ResolutionStrategy, ResolvedPackage,
    UpdateSummary, dry_run_update, dry_run_update_partial, resolve_for_lockfile,
    resolve_for_lockfile_partial, unsupported_repo_warnings,
};
