//! Spot-check the Constraint→Range conversion against
//! [`Constraint::matches`]. The two paths must agree on every
//! version under test — that's the whole point of the asymmetric
//! boundary encoding.

use super::to_range;
use composer_semver::constraint::Constraint;
use composer_semver::version::Version;

fn parse_v(s: &str) -> Version {
    Version::parse(s).unwrap_or_else(|e| panic!("parse version {s:?}: {e}"))
}

fn parse_c(s: &str) -> Constraint {
    Constraint::parse(s).unwrap_or_else(|e| panic!("parse constraint {s:?}: {e}"))
}

/// Anchor: for each (constraint, version) pair, `to_range` membership
/// matches `Constraint::matches` for the same inputs. This is the
/// whole-system property the encoding is supposed to satisfy.
fn assert_agree(constraint_str: &str, version_str: &str, expected: bool) {
    let c = parse_c(constraint_str);
    let v = parse_v(version_str);
    let r = to_range(&c);
    let in_range = r.contains(&v);
    let matches = c.matches(&v);
    assert_eq!(
        in_range, expected,
        "{constraint_str:?} contains({version_str:?}) = {in_range}, expected {expected}"
    );
    assert_eq!(
        matches, expected,
        "{constraint_str:?}.matches({version_str:?}) = {matches}, expected {expected}"
    );
}

#[test]
fn carets_admit_same_numeric_prereleases() {
    // The asymmetric explicit-lower rule.
    assert_agree("^1.2.3", "1.2.3", true);
    assert_agree("^1.2.3", "1.2.3-beta", true);
    assert_agree("^1.2.3", "1.9.0", true);
    assert_agree("^1.2.3", "2.0.0", false);
    assert_agree("^1.2.3", "2.0.0-alpha", false);
}

#[test]
fn partial_one_rejects_same_numeric_prereleases() {
    // `1` is the synthesized-lower-bound case. Same numeric body as
    // 1.0.0 → reject 1.0.0-beta.
    assert_agree("1", "1.0.0", true);
    assert_agree("1", "1.5.2", true);
    assert_agree("1", "1.0.0-beta", false);
    assert_agree("1", "2.0.0", false);
    assert_agree("1", "2.0.0-beta", false);
}

#[test]
fn strict_upper_bound_excludes_prereleases_of_boundary() {
    assert_agree("<1.2.3", "1.2.2", true);
    assert_agree("<1.2.3", "1.2.3", false);
    assert_agree("<1.2.3", "1.2.3-beta", false);
    assert_agree("<2.0.0", "2.0.0-alpha", false);
}

#[test]
fn inclusive_upper_bound_admits_prereleases_of_boundary() {
    assert_agree("<=1.2.3", "1.2.3", true);
    assert_agree("<=1.2.3", "1.2.3-beta", true);
    assert_agree("<=1.2.3", "1.2.2", true);
    assert_agree("<=1.2.3", "1.2.4", false);
}

#[test]
fn tilde_full_admits_same_numeric_prereleases() {
    // `~1.2.3` → `>=1.2.3, <1.3.0`. With explicit-lower flag set, the
    // lower bound admits prereleases of 1.2.3.
    assert_agree("~1.2.3", "1.2.3", true);
    assert_agree("~1.2.3", "1.2.3-beta", true);
    assert_agree("~1.2.3", "1.2.99", true);
    assert_agree("~1.2.3", "1.3.0", false);
}

#[test]
fn or_unions_cleanly() {
    assert_agree("^1.2.0 || ^2.0", "1.5.0", true);
    assert_agree("^1.2.0 || ^2.0", "2.7.0", true);
    assert_agree("^1.2.0 || ^2.0", "3.0.0", false);
    assert_agree("^1.2.0 || ^2.0", "1.1.99", false);
}

#[test]
fn any_matches_everything_numeric() {
    let r = to_range(&parse_c("*"));
    for v in ["0.0.0", "1.2.3", "1.0.0-beta", "99.0.0"] {
        assert!(r.contains(&parse_v(v)), "* should contain {v}");
    }
}

#[test]
fn wildcard_pattern_acts_as_minor_range() {
    assert_agree("1.2.*", "1.2.0", true);
    assert_agree("1.2.*", "1.2.99", true);
    assert_agree("1.2.*", "1.3.0", false);
    assert_agree("1.2.*", "1.2.0-beta", false); // wildcard is synthesized
}
