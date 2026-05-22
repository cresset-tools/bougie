//! Drives `bougie-autoloader`'s `normalize_version` against the
//! ground-truth `Composer\Semver\VersionParser::normalize` output
//! captured in `tests/data/version_normalize.tsv`.
//!
//! Re-generate the TSV with `scripts/gen-version-normalize-fixtures.php`
//! whenever the pinned composer/semver version changes.
//!
//! Three test outcomes:
//! - **`matches_composer_for_normalizable_inputs`**: for every input
//!   Composer accepts, bougie returns `Ok(<same string>)`.
//! - **`errors_on_inputs_composer_rejects`**: for every input that
//!   makes Composer throw, bougie returns `Err`.
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
        match normalize_version(c.input) {
            Ok(actual) if actual == expected => {}
            Ok(actual) => failures.push(format!(
                "input={input:?} expected={expected:?} actual={actual:?}",
                input = c.input,
                expected = expected,
                actual = actual,
            )),
            Err(e) => failures.push(format!(
                "input={input:?} expected={expected:?} got_error={err}",
                input = c.input,
                expected = expected,
                err = e,
            )),
        }
    }
    assert!(failures.is_empty(), 
        "{} version inputs diverge from Composer's normalize() output:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

#[test]
fn errors_on_inputs_composer_rejects() {
    let mut failures: Vec<String> = Vec::new();
    for c in cases() {
        let Expectation::Throws(_msg) = c.expected else {
            continue;
        };
        match normalize_version(c.input) {
            Err(_) => {}
            Ok(actual) => failures.push(format!(
                "input={input:?} expected error, but got Ok({actual:?})",
                input = c.input,
                actual = actual,
            )),
        }
    }
    assert!(failures.is_empty(), 
        "{} inputs Composer rejects, bougie accepted:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}
