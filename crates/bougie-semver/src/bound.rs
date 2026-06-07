//! Constraint bounds — port of `Composer\Semver\Constraint\Bound`.
//!
//! A [`Bound`] is one end of a version interval: a normalized version
//! string plus an inclusivity flag. Composer derives the lower/upper
//! bound of a constraint (`Constraint::getLowerBound` /
//! `MultiConstraint::extractBounds`) and the autoloader's
//! `platform_check.php` generator uses the *lowest* PHP bound across
//! every `php` / `php-64bit` requirement to emit the
//! `PHP_VERSION_ID >= N` guard.
//!
//! The two sentinels match Composer's `Bound::zero()` /
//! `Bound::positiveInfinity()` byte-for-byte so [`Bound::is_zero`] /
//! [`Bound::is_positive_infinity`] reproduce the upstream short-circuits.

use crate::version::Version;
use std::cmp::Ordering;

/// PHP's `PHP_INT_MAX` on a 64-bit build — the version body Composer
/// uses for the positive-infinity sentinel.
const PHP_INT_MAX: &str = "9223372036854775807";

/// One end of a version interval. Mirrors `Composer\Semver\Constraint\Bound`:
/// a normalized version string and whether the endpoint itself is
/// included in the interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    /// Normalized version string (what `Version::normalized` produces),
    /// or one of the two sentinel bodies (`0.0.0.0-dev` /
    /// `<PHP_INT_MAX>.0.0.0`).
    version: String,
    is_inclusive: bool,
}

impl Bound {
    /// Construct a bound from a normalized version string.
    #[must_use]
    pub fn new(version: String, is_inclusive: bool) -> Self {
        Self { version, is_inclusive }
    }

    /// Composer's `Bound::zero()` — `0.0.0.0-dev` inclusive. The lower
    /// bound of any constraint with no real floor (`<X`, `!=X`, `*`).
    #[must_use]
    pub fn zero() -> Self {
        Self { version: "0.0.0.0-dev".to_owned(), is_inclusive: true }
    }

    /// Composer's `Bound::positiveInfinity()` — `<PHP_INT_MAX>.0.0.0`
    /// exclusive. The upper bound of any constraint with no real ceiling
    /// (`>X`, `>=X`, `!=X`, `*`).
    #[must_use]
    pub fn positive_infinity() -> Self {
        Self { version: format!("{PHP_INT_MAX}.0.0.0"), is_inclusive: false }
    }

    /// The normalized version body.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Whether the endpoint is part of the interval.
    #[must_use]
    pub fn is_inclusive(&self) -> bool {
        self.is_inclusive
    }

    /// Matches `Bound::isZero()`.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.version == "0.0.0.0-dev" && self.is_inclusive
    }

    /// Matches `Bound::isPositiveInfinity()`.
    #[must_use]
    pub fn is_positive_infinity(&self) -> bool {
        self.version == format!("{PHP_INT_MAX}.0.0.0") && !self.is_inclusive
    }

    /// Port of `Bound::compareTo($other, $operator)` with `$operator`
    /// expressed as `gt` (`true` → `'>'`, `false` → `'<'`). Returns
    /// whether `self` lies strictly beyond `other` in the requested
    /// direction; two structurally-equal bounds compare `false` (as in
    /// Composer's `if ($this == $other) return false`).
    #[must_use]
    pub fn compare_to(&self, other: &Bound, gt: bool) -> bool {
        if self == other {
            return false;
        }
        match self.version_cmp(other) {
            Ordering::Greater => gt,
            Ordering::Less => !gt,
            // Equal versions, differing inclusivity: Composer returns
            // `$other->isInclusive()` for `'>'`, `!$other->isInclusive()`
            // for `'<'`.
            Ordering::Equal => {
                if gt {
                    other.is_inclusive
                } else {
                    !other.is_inclusive
                }
            }
        }
    }

    /// Order the two endpoints purely by version body (ignoring
    /// inclusivity), the way PHP's `version_compare` would for these
    /// normalized strings. The sentinels parse like any other numeric
    /// version (`0.0.0.0-dev` sorts below every real version; the
    /// `PHP_INT_MAX` body sorts above), so a plain [`Version`]
    /// comparison reproduces the upstream ordering.
    #[must_use]
    pub fn version_cmp(&self, other: &Bound) -> Ordering {
        match (Version::parse(&self.version), Version::parse(&other.version)) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            // Both sentinel bodies and every normalized version parse,
            // so this fallback is unreachable in practice; lexical
            // ordering keeps the function total.
            _ => self.version.cmp(&other.version),
        }
    }
}
