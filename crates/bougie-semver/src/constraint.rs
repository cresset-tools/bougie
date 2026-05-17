//! Composer-flavored constraint.
//!
//! **Status:** placeholder. Phase B of `bougie-composer-resolver`
//! provides the parser, the `Range<Version>` form for pubgrub
//! consumption, and the `matches`/`intersect`/`union` operations.

use crate::version::Version;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constraint {
    raw: String,
}

impl Constraint {
    /// Parse a Composer constraint string (e.g. `^1.2`, `~2.5`, `>=1 <2`,
    /// `1.0 || 2.0`, `1.2.*`).
    ///
    /// **Not yet implemented.**
    pub fn parse(_s: &str) -> Result<Self, ParseError> {
        Err(ParseError::Unimplemented)
    }

    /// Return whether `version` satisfies this constraint.
    ///
    /// **Not yet implemented.**
    pub fn matches(&self, _version: &Version) -> bool {
        false
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Unimplemented,
    Invalid(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unimplemented => write!(f, "constraint parsing not yet implemented"),
            Self::Invalid(s) => write!(f, "invalid constraint: {s}"),
        }
    }
}

impl std::error::Error for ParseError {}
