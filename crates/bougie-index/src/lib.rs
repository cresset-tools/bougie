//! Index protocol consumer (CLI.md §7).

pub mod fetch;
pub mod verify;
pub mod wire;

pub use fetch::{FetchOutcome, FetchedRoot, fetch_root};
pub use verify::{
    DetachedEcdsa, EXPECTED_ISSUER, EXPECTED_REPOSITORY, SigstoreBundleVerifier, TrustDescription,
    Verifier, build_verifier, describe_trust,
};
pub use wire::{Artifact, Closure, Manifest, Root, Section, SectionRef};

/// Map an index host URL to a filesystem-safe directory name for the
/// per-host cache layout under `$BOUGIE_CACHE/index/<host>/`. Strips
/// the scheme and replaces path-unsafe characters.
#[must_use]
pub fn host_to_dirname(host: &str) -> String {
    let h = host.trim_end_matches('/');
    let stripped = h
        .strip_prefix("https://")
        .or_else(|| h.strip_prefix("http://"))
        .unwrap_or(h);
    stripped.replace(['/', ':'], "_")
}
