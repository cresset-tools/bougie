//! Match an exact [`Version`] against a [`VersionLike`] spec — a bare
//! partial version (`8.3`, prefix-match) or a Composer constraint
//! (`^8.3`, `>=8.1,<8.4`).
//!
//! Shared by every place that filters a set of concrete PHP versions
//! (installed managed builds, discovered system PHPs, index manifests)
//! against a user/`require.php` spec, so they all agree on semantics.

use crate::request::VersionLike;
use crate::version::{PartialVersion, Version};

/// Whether `v` satisfies `spec`.
pub fn version_satisfies(v: &Version, spec: &VersionLike) -> bool {
    match spec {
        VersionLike::Version(pv) => matches_partial(v, pv),
        VersionLike::Constraint(c) => {
            // Constraint matching is defined against the semver-shaped
            // Version. Lift bougie's exact triple into a
            // Composer-normalized "X.Y.Z" — the same trick the resolver
            // uses.
            let Ok(lifted) = composer_semver::Version::parse(&v.to_string()) else {
                return false;
            };
            c.matches(&lifted)
        }
    }
}

/// Prefix-match an exact version against a partial: components present
/// in `pv` must equal `v`'s; absent components are wildcards.
pub fn matches_partial(v: &Version, pv: &PartialVersion) -> bool {
    if v.major != pv.major {
        return false;
    }
    if let Some(m) = pv.minor
        && v.minor != m
    {
        return false;
    }
    if let Some(p) = pv.patch
        && v.patch != p
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use composer_semver::Constraint;

    fn pv(major: u32, minor: Option<u32>, patch: Option<u32>) -> VersionLike {
        VersionLike::Version(PartialVersion { major, minor, patch })
    }

    fn con(s: &str) -> VersionLike {
        VersionLike::Constraint(Constraint::parse(s).unwrap())
    }

    #[test]
    fn partial_prefix_matches() {
        let v = Version::new(8, 3, 12);
        assert!(version_satisfies(&v, &pv(8, None, None)));
        assert!(version_satisfies(&v, &pv(8, Some(3), None)));
        assert!(version_satisfies(&v, &pv(8, Some(3), Some(12))));
        assert!(!version_satisfies(&v, &pv(8, Some(4), None)));
        assert!(!version_satisfies(&v, &pv(7, None, None)));
        assert!(!version_satisfies(&v, &pv(8, Some(3), Some(11))));
    }

    #[test]
    fn constraint_matches() {
        let v = Version::new(8, 3, 12);
        assert!(version_satisfies(&v, &con("^8.3")));
        assert!(version_satisfies(&v, &con(">=8.1,<8.4")));
        assert!(!version_satisfies(&v, &con("^8.4")));
        assert!(!version_satisfies(&v, &con(">=8.1,<8.3")));
    }
}
