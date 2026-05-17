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
