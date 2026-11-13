//! The git error classifier. Installing git sources moved to
//! `composer-install`, whose tests cover the clone + checkout path.

use super::*;

/// Downcast a report to the typed VCS error and return its parts.
fn vcs_parts(report: &eyre::Report) -> (String, String) {
    let err = report
        .downcast_ref::<bougie_errors::BougieError>()
        .expect("a BougieError::Vcs");
    match err {
        bougie_errors::BougieError::Vcs {
            operation, hint, ..
        } => (operation.clone(), hint.clone()),
        other => panic!("expected Vcs, got {other:?}"),
    }
}

#[test]
fn classifier_maps_auth_and_missing_ref_stderr() {
    let auth = classify_git(
        "clone",
        "https://host/x.git",
        "remote: Authentication failed for 'x'",
    );
    let (_op, hint) = vcs_parts(&auth);
    assert!(
        hint.contains("credential") || hint.contains("bougie login"),
        "{hint}"
    );

    let missing = classify_git("checkout", "u", "fatal: reference is not a tree: deadbeef");
    let (_op, hint) = vcs_parts(&missing);
    assert!(hint.contains("bougie update"), "{hint}");

    // Every classified error maps to the VCS exit code.
    assert_eq!(bougie_errors::exit_code_for(&auth), 71);
}

#[test]
fn git_missing_error_hints_install() {
    let e = git_missing("invocation");
    let (op, hint) = vcs_parts(&e);
    assert_eq!(op, "invocation");
    assert!(hint.contains("install git"), "{hint}");
}
