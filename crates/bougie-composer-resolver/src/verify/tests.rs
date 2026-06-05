//! End-to-end tests for `verify_lock`. Constructs tempdir projects
//! with hand-written composer.json + composer.lock and checks the
//! returned `VerifyOutcome`.

use super::*;
use bougie_composer::lockfile;
use std::path::Path;
use tempfile::TempDir;

fn write_project(dir: &Path, composer_json: &str, composer_lock: &str) {
    std::fs::write(dir.join("composer.json"), composer_json).unwrap();
    std::fs::write(dir.join("composer.lock"), composer_lock).unwrap();
}

fn hash_for(composer_json: &str) -> String {
    lockfile::content_hash(composer_json.as_bytes()).unwrap()
}

/// A composer.json + composer.lock pair where the root requires
/// `acme/foo: ^1.2`, the lock has `acme/foo` at `1.2.3`, and that
/// package has no further deps. Valid by construction.
fn valid_pair() -> (String, String) {
    let composer_json = r#"{
        "name": "test/valid",
        "require": {"acme/foo": "^1.2"}
    }"#;
    let hash = hash_for(composer_json);
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [
                {{
                    "name": "acme/foo",
                    "version": "1.2.3",
                    "dist": {{"type": "zip", "url": "https://e/f.zip", "shasum": "aa"}}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    (composer_json.to_owned(), lock)
}

#[test]
fn valid_pair_returns_valid() {
    let tmp = TempDir::new().unwrap();
    let (cj, cl) = valid_pair();
    write_project(tmp.path(), &cj, &cl);
    match verify_lock(tmp.path(), VerifyOptions::default()).unwrap() {
        VerifyOutcome::Valid => {}
        VerifyOutcome::Invalid { reason } => panic!("expected valid, got: {reason}"),
    }
}

#[test]
fn content_hash_mismatch_is_reported() {
    let tmp = TempDir::new().unwrap();
    let composer_json = r#"{"name": "x", "require": {}}"#;
    let lock = r#"{
        "content-hash": "0000000000000000000000000000000a",
        "packages": [],
        "packages-dev": []
    }"#;
    write_project(tmp.path(), composer_json, lock);
    let VerifyOutcome::Invalid { reason } =
        verify_lock(tmp.path(), VerifyOptions::default()).unwrap()
    else {
        panic!("expected invalid");
    };
    assert!(reason.contains("out of sync"), "{reason}");
}

#[test]
fn missing_lock_is_reported() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("composer.json"), r#"{"name":"x","require":{}}"#).unwrap();
    let VerifyOutcome::Invalid { reason } =
        verify_lock(tmp.path(), VerifyOptions::default()).unwrap()
    else {
        panic!("expected invalid");
    };
    assert!(reason.contains("composer.lock"), "{reason}");
    assert!(reason.contains("composer update"), "{reason}");
}

#[test]
fn missing_composer_json_errors() {
    let tmp = TempDir::new().unwrap();
    let err = verify_lock(tmp.path(), VerifyOptions::default()).expect_err("must error");
    let msg = format!("{err:#}");
    assert!(msg.contains("not a Composer project"), "{msg}");
}

#[test]
fn version_violating_root_require_is_reported() {
    // Root requires ^2.0 but lock pins 1.5. Pubgrub should reject
    // and produce a derivation tree mentioning the conflict.
    let tmp = TempDir::new().unwrap();
    let composer_json = r#"{"name":"x","require":{"acme/foo":"^2.0"}}"#;
    let hash = hash_for(composer_json);
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [
                {{
                    "name": "acme/foo",
                    "version": "1.5.0",
                    "dist": {{"type":"zip","url":"https://e/f.zip","shasum":"aa"}}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project(tmp.path(), composer_json, &lock);
    let VerifyOutcome::Invalid { reason } =
        verify_lock(tmp.path(), VerifyOptions::default()).unwrap()
    else {
        panic!("expected invalid");
    };
    assert!(reason.contains("acme/foo"), "must name the package: {reason}");
}

#[test]
fn missing_transitive_dep_is_reported() {
    // acme/foo declares require {acme/bar: ^1.0} but no acme/bar
    // in the lock. The verifier should reject with a derivation
    // mentioning acme/bar.
    let tmp = TempDir::new().unwrap();
    let composer_json = r#"{"name":"x","require":{"acme/foo":"^1.0"}}"#;
    let hash = hash_for(composer_json);
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [
                {{
                    "name": "acme/foo",
                    "version": "1.0.0",
                    "require": {{"acme/bar": "^1.0"}},
                    "dist": {{"type":"zip","url":"https://e/f.zip","shasum":"aa"}}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project(tmp.path(), composer_json, &lock);
    let VerifyOutcome::Invalid { reason } =
        verify_lock(tmp.path(), VerifyOptions::default()).unwrap()
    else {
        panic!("expected invalid");
    };
    assert!(reason.contains("acme/bar"), "must name the missing transitive: {reason}");
}

#[test]
fn transitive_version_conflict_is_reported() {
    // Root requires acme/foo ^1.0 AND acme/bar ^1.0. acme/foo requires
    // acme/bar ^2.0. acme/bar is locked at 1.5 (satisfies root but not
    // acme/foo's transitive). Verifier should reject.
    let tmp = TempDir::new().unwrap();
    let composer_json = r#"{"name":"x","require":{"acme/foo":"^1.0","acme/bar":"^1.0"}}"#;
    let hash = hash_for(composer_json);
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [
                {{
                    "name": "acme/foo",
                    "version": "1.0.0",
                    "require": {{"acme/bar": "^2.0"}},
                    "dist": {{"type":"zip","url":"https://e/f.zip","shasum":"aa"}}
                }},
                {{
                    "name": "acme/bar",
                    "version": "1.5.0",
                    "dist": {{"type":"zip","url":"https://e/b.zip","shasum":"bb"}}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project(tmp.path(), composer_json, &lock);
    let VerifyOutcome::Invalid { reason } =
        verify_lock(tmp.path(), VerifyOptions::default()).unwrap()
    else {
        panic!("expected invalid");
    };
    assert!(reason.contains("acme/bar"), "{reason}");
}

#[test]
fn platform_requirements_are_skipped() {
    // No `.bougie/state/resolved` pin → PlatformEnv models nothing, so
    // even `php` is left unvalidated (and `ext-*` is never modeled).
    // Verifier passes since there are no other packages. The pinned
    // cases are covered by the `php_*` tests below (#118).
    let tmp = TempDir::new().unwrap();
    let composer_json = r#"{
        "name": "x",
        "require": {"php": "^8.3", "ext-redis": "*"}
    }"#;
    let hash = hash_for(composer_json);
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [],
            "packages-dev": []
        }}"#
    );
    write_project(tmp.path(), composer_json, &lock);
    match verify_lock(tmp.path(), VerifyOptions::default()).unwrap() {
        VerifyOutcome::Valid => {}
        VerifyOutcome::Invalid { reason } => panic!("expected valid, got: {reason}"),
    }
}

#[test]
fn no_dev_skips_dev_only_lock_failures() {
    // packages-dev contains a violation, but --no-dev hides it from
    // the verifier so the rest of the lock passes.
    let tmp = TempDir::new().unwrap();
    let composer_json = r#"{"name":"x","require":{},"require-dev":{"dev/x":"^2.0"}}"#;
    let hash = hash_for(composer_json);
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [],
            "packages-dev": [
                {{
                    "name": "dev/x",
                    "version": "1.0.0",
                    "dist": {{"type":"zip","url":"https://e/x.zip","shasum":"aa"}}
                }}
            ]
        }}"#
    );
    write_project(tmp.path(), composer_json, &lock);
    // Without --no-dev: dev/x violation → invalid.
    let VerifyOutcome::Invalid { .. } =
        verify_lock(tmp.path(), VerifyOptions { no_dev: false }).unwrap()
    else {
        panic!("expected invalid with dev included");
    };
    // With --no-dev: no requires at all → trivially valid.
    match verify_lock(tmp.path(), VerifyOptions { no_dev: true }).unwrap() {
        VerifyOutcome::Valid => {}
        VerifyOutcome::Invalid { reason } => panic!("expected valid with --no-dev, got: {reason}"),
    }
}

/// Write a `.bougie/state/resolved` PHP pin so `verify_lock` models the
/// `php` platform package (#118).
fn write_pin(dir: &Path, version_flavor: &str) {
    let state = dir.join(".bougie").join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("resolved"), version_flavor).unwrap();
}

#[test]
fn php_root_require_satisfied_by_pin_is_valid() {
    // Pinned PHP 8.3.31 satisfies a root `php: ^8.3` require.
    let tmp = TempDir::new().unwrap();
    write_pin(tmp.path(), "8.3.31-nts");
    let composer_json = r#"{"name":"x","require":{"php":"^8.3"}}"#;
    let hash = hash_for(composer_json);
    let lock = format!(r#"{{"content-hash":"{hash}","packages":[],"packages-dev":[]}}"#);
    write_project(tmp.path(), composer_json, &lock);
    match verify_lock(tmp.path(), VerifyOptions::default()).unwrap() {
        VerifyOutcome::Valid => {}
        VerifyOutcome::Invalid { reason } => panic!("expected valid, got: {reason}"),
    }
}

#[test]
fn php_root_require_violating_pin_is_reported() {
    // Pinned PHP 8.3.31 does NOT satisfy a root `php: >=8.4` require —
    // exactly the case that used to resolve silently before #118.
    let tmp = TempDir::new().unwrap();
    write_pin(tmp.path(), "8.3.31-nts");
    let composer_json = r#"{"name":"x","require":{"php":">=8.4"}}"#;
    let hash = hash_for(composer_json);
    let lock = format!(r#"{{"content-hash":"{hash}","packages":[],"packages-dev":[]}}"#);
    write_project(tmp.path(), composer_json, &lock);
    let VerifyOutcome::Invalid { reason } =
        verify_lock(tmp.path(), VerifyOptions::default()).unwrap()
    else {
        panic!("expected invalid: pinned 8.3 cannot satisfy php >=8.4");
    };
    assert!(reason.contains("php"), "derivation must name php: {reason}");
}

#[test]
fn transitive_php_require_violating_pin_is_reported() {
    // A locked dependency requires php >=8.4 while the project is pinned
    // to 8.3.31 — the lock can't actually install. (This is the shape of
    // the real Mage-OS bug: endroid/qr-code 6.x needs PHP 8.4.)
    let tmp = TempDir::new().unwrap();
    write_pin(tmp.path(), "8.3.31-nts");
    let composer_json = r#"{"name":"x","require":{"acme/foo":"^1.0"}}"#;
    let hash = hash_for(composer_json);
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [
                {{
                    "name": "acme/foo",
                    "version": "1.0.0",
                    "require": {{"php": ">=8.4"}},
                    "dist": {{"type":"zip","url":"https://e/f.zip","shasum":"aa"}}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project(tmp.path(), composer_json, &lock);
    let VerifyOutcome::Invalid { reason } =
        verify_lock(tmp.path(), VerifyOptions::default()).unwrap()
    else {
        panic!("expected invalid: acme/foo needs php >=8.4 but pin is 8.3");
    };
    assert!(reason.contains("php"), "derivation must name php: {reason}");
}
