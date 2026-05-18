//! Drives `bougie-autoloader`'s `normalize_version` against the
//! ground-truth `Composer\Semver\VersionParser::normalize` output
//! captured in `tests/data/version_normalize.tsv`.
//!
//! Re-generate the TSV with `scripts/gen-version-normalize-fixtures.php`
//! whenever the pinned composer/semver version changes.
//!
//! Three test outcomes:
//! - **`matches_composer_for_normalizable_inputs`**: bougie's output
//!   equals Composer's for every input Composer accepts. Currently
//!   asserts; this is the failing test the port has to drive to green.
//! - **`reports_throws_inputs_separately`**: collects every input
//!   Composer rejects and either bougie rejects too (once the API
//!   gains an error path) or bougie produces a value (today's
//!   behavior — flagged with a warning so we don't lose track).
//! - **`fixture_is_well_formed`**: every line is either a comment or
//!   `input\toutput` / `input\tTHROWS\tmessage`. Cheap sanity check on
//!   the TSV format.
//!
//! Failures print every diverging case at once so one run of
//! `cargo test` produces the full punch-list.

use bougie_autoloader::test_api::normalize_version;

const TSV: &str = include_str!("data/version_normalize.tsv");

#[derive(Debug)]
struct Case<'a> {
    input: &'a str,
    expected: Expectation<'a>,
}

#[derive(Debug)]
enum Expectation<'a> {
    Normalized(&'a str),
    Throws(&'a str),
}

fn cases() -> Vec<Case<'static>> {
    let mut out = Vec::new();
    for raw in TSV.lines() {
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let mut fields = raw.splitn(3, '\t');
        let input = fields.next().expect("input column");
        let second = fields.next().expect("output or THROWS column");
        let expected = if second == "THROWS" {
            let msg = fields.next().unwrap_or("");
            Expectation::Throws(msg)
        } else {
            Expectation::Normalized(second)
        };
        out.push(Case { input, expected });
    }
    out
}

#[test]
fn fixture_is_well_formed() {
    let cases = cases();
    assert!(!cases.is_empty(), "no test cases parsed from TSV");
    for c in &cases {
        // Inputs may contain leading whitespace (the trim() test
        // case), but never tabs.
        assert!(
            !c.input.contains('\t'),
            "input should not contain tabs: {:?}",
            c.input
        );
    }
}

#[test]
fn matches_composer_for_normalizable_inputs() {
    let mut failures: Vec<String> = Vec::new();
    for c in cases() {
        let Expectation::Normalized(expected) = c.expected else {
            continue;
        };
        let actual = normalize_version(c.input);
        if actual != expected {
            failures.push(format!(
                "input={input:?} expected={expected:?} actual={actual:?}",
                input = c.input,
                expected = expected,
                actual = actual,
            ));
        }
    }
    if !failures.is_empty() {
        panic!(
            "{} version inputs diverge from Composer's normalize() output:\n  {}",
            failures.len(),
            failures.join("\n  ")
        );
    }
}

#[test]
fn reports_throws_inputs_separately() {
    // The bougie API currently returns String unconditionally, so
    // there's no error-path to assert against. Track each THROWS case
    // so the punch-list survives until `normalize_version` gains a
    // Result-shaped return type.
    let mut throws_observed: Vec<String> = Vec::new();
    for c in cases() {
        let Expectation::Throws(msg) = c.expected else {
            continue;
        };
        let actual = normalize_version(c.input);
        throws_observed.push(format!(
            "input={input:?} composer-throws-with={msg:?} bougie-returns={actual:?}",
            input = c.input,
            msg = msg,
            actual = actual,
        ));
    }
    // Once bougie's normalizer rejects these inputs (whatever shape
    // that takes — Result, Option, panic), update this test to assert
    // the rejection. For now we just confirm the fixture carries
    // at-least-one throws-case so we don't accidentally lose coverage.
    assert!(
        !throws_observed.is_empty(),
        "fixture has no THROWS cases — did the TSV regenerate cleanly?"
    );
}
