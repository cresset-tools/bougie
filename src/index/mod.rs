//! Index protocol consumer (CLI.md §7).

pub mod fetch;
pub mod verify;
pub mod wire;

pub use fetch::{fetch_root, FetchOutcome, FetchedRoot};
pub use verify::{Sigstore, TrustRoot, Verifier};
pub use wire::{Artifact, Closure, Manifest, Root, Section, SectionRef};
