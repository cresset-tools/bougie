//! Composer-flavored version.
//!
//! **Status:** placeholder. The full normalization rules
//! (4-segment expansion, stability suffix parsing, `dev-*` namespace,
//! branch aliasing) land alongside Phase B of `bougie-composer-resolver`.
//! Layer 1 conformance fixtures in `tests/data/conformance.json` drive
//! the implementation forward.

use crate::stability::Stability;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub extra: u32,
    pub stability: Stability,
    pub stability_num: u16,
}

impl Version {
    /// Parse a Composer-normalized version string.
    ///
    /// **Not yet implemented.** Per `RESOLVER_PLAN.md`, this is Phase B
    /// scope. The signature is fixed so callers and conformance tests
    /// can be written against it.
    pub fn parse(_s: &str) -> Result<Self, ParseError> {
        Err(ParseError::Unimplemented)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Reserved for the period before Phase B lands. Tests should treat
    /// this as "not yet implemented" rather than "input was invalid."
    Unimplemented,
    Invalid(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unimplemented => write!(f, "version parsing not yet implemented"),
            Self::Invalid(s) => write!(f, "invalid version: {s}"),
        }
    }
}

impl std::error::Error for ParseError {}
