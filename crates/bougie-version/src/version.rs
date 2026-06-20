//! Bougie's exact-triple PHP version + `<major>.<minor>?.<patch>?`
//! partial-version types.
//!
//! Composer-flavored constraints (the full grammar Composer accepts in
//! `require.php`: wildcards, hyphen ranges, stability suffixes, etc.)
//! live in the [`composer_semver`] crate. This module keeps only the
//! narrow exact-triple shape bougie's own index and on-disk install
//! layout use — every bougie artifact version is `<u32>.<u32>.<u32>`
//! by construction, and there's no need for the wider grammar here.

use eyre::{eyre, Result};
use std::fmt;
use std::str::FromStr;

/// A partially-specified version: major, optional minor, optional patch.
/// Matches the `<version>` form in CLI.md §3.5.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PartialVersion {
    pub major: u32,
    pub minor: Option<u32>,
    pub patch: Option<u32>,
}

impl PartialVersion {
    pub fn is_exact(&self) -> bool {
        self.minor.is_some() && self.patch.is_some()
    }

    pub fn parse(s: &str) -> Result<Self> {
        if s.is_empty() {
            return Err(eyre!("empty version"));
        }
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() > 3 {
            return Err(eyre!("version has too many components: {s}"));
        }
        let major = parse_component(parts[0], "major")?;
        let minor = if parts.len() >= 2 {
            Some(parse_component(parts[1], "minor")?)
        } else {
            None
        };
        let patch = if parts.len() == 3 {
            Some(parse_component(parts[2], "patch")?)
        } else {
            None
        };
        Ok(Self { major, minor, patch })
    }
}

fn parse_component(s: &str, label: &str) -> Result<u32> {
    s.parse::<u32>()
        .map_err(|_| eyre!("invalid {label} version component: {s:?}"))
}

impl PartialVersion {
    /// Pad missing minor/patch to 0 to get a fully-qualified version
    /// suitable for ordering comparisons.
    pub fn pad(&self) -> Version {
        Version {
            major: self.major,
            minor: self.minor.unwrap_or(0),
            patch: self.patch.unwrap_or(0),
        }
    }
}

impl fmt::Display for PartialVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.major)?;
        if let Some(m) = self.minor {
            write!(f, ".{m}")?;
        }
        if let Some(p) = self.patch {
            write!(f, ".{p}")?;
        }
        Ok(())
    }
}

/// Fully-qualified version: every component present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self { major, minor, patch }
    }
}

impl FromStr for Version {
    type Err = eyre::Report;
    fn from_str(s: &str) -> Result<Self> {
        let pv = PartialVersion::parse(s)?;
        if !pv.is_exact() {
            return Err(eyre!("not an exact version: {s}"));
        }
        Ok(pv.pad())
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pv(major: u32, minor: Option<u32>, patch: Option<u32>) -> PartialVersion {
        PartialVersion { major, minor, patch }
    }

    #[test]
    fn parse_versions() {
        assert_eq!(PartialVersion::parse("8").unwrap(), pv(8, None, None));
        assert_eq!(PartialVersion::parse("8.3").unwrap(), pv(8, Some(3), None));
        assert_eq!(
            PartialVersion::parse("8.3.12").unwrap(),
            pv(8, Some(3), Some(12))
        );
    }

    #[test]
    fn version_display_round_trips() {
        for s in ["8", "8.3", "8.3.12"] {
            assert_eq!(PartialVersion::parse(s).unwrap().to_string(), s);
        }
    }

    #[test]
    fn version_rejects_garbage() {
        assert!(PartialVersion::parse("").is_err());
        assert!(PartialVersion::parse("8.3.12.4").is_err());
        assert!(PartialVersion::parse("8.x").is_err());
        assert!(PartialVersion::parse("v8.3").is_err());
    }

    #[test]
    fn version_from_str_requires_full_triple() {
        assert!(Version::from_str("8.3.12").is_ok());
        assert!(Version::from_str("8.3").is_err());
    }
}
