//! Composer-flavored semver primitives.
//!
//! This crate exists as the shared substrate between `bougie-resolver`
//! (PHP/ext picker) and the future `bougie-composer-resolver` (Composer
//! dep solver). Per `RESOLVER_PLAN.md`, both need the same constraint
//! algebra — defining it once here avoids drift.
//!
//! **Status:** scaffolding only. The Version and Constraint types are
//! placeholders; implementations land with Phase B of the resolver.
//! Layer 1 conformance fixtures (see `tests/data/conformance.json`)
//! are already committed so that as the implementations grow, the
//! pass/fail signal is loud and locatable.

pub mod bound;
pub mod constraint;
pub mod stability;
pub mod version;

pub use bound::Bound;
pub use constraint::Constraint;
pub use stability::Stability;
pub use version::Version;
