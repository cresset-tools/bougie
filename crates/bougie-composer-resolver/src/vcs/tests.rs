#![cfg(unix)]
//! Hermetic git tests: build a throwaway repo in a tempdir and drive the
//! plumbing against it via a `file://` URL — no network.

use std::path::Path;
use std::process::Command;

use bougie_paths::Paths;
use tempfile::TempDir;

use super::*;

/// Run `git -C dir <args>` with a fixed identity and no ambient config,
/// asserting success. Used only to build fixtures.
fn git_ok(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=Test",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn rev_parse(dir: &Path, rev: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", rev])
        .output()
        .expect("git rev-parse");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Create a git repo with two commits (tagged `v1.0.0` at the first) and
/// return `(repo_dir, url, first_sha, second_sha)`.
fn fixture_repo(root: &Path) -> (std::path::PathBuf, String, String, String) {
    let repo = root.join("origin");
    std::fs::create_dir_all(&repo).unwrap();
    git_ok(&repo, &["init", "-q"]);
    std::fs::write(repo.join("composer.json"), r#"{"name":"acme/lib"}"#).unwrap();
    std::fs::write(repo.join("first.php"), "<?php // v1\n").unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-q", "-m", "first"]);
    git_ok(&repo, &["tag", "v1.0.0"]);
    let first = rev_parse(&repo, "HEAD");

    std::fs::write(repo.join("second.php"), "<?php // v2\n").unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-q", "-m", "second"]);
    let second = rev_parse(&repo, "HEAD");

    let url = format!("file://{}", repo.display());
    (repo, url, first, second)
}

fn paths_in(tmp: &Path) -> Paths {
    Paths::new(tmp.join("home"), tmp.join("cache"))
}

#[test]
fn install_source_checks_out_exact_reference() {
    let tmp = TempDir::new().unwrap();
    let (_repo, url, first, _second) = fixture_repo(tmp.path());
    let paths = paths_in(tmp.path());

    // Check out the FIRST commit — the tagged one, before second.php.
    let dest = tmp.path().join("vendor/acme/lib");
    install_source(&paths, &url, &first, &dest).unwrap();

    assert!(dest.join("composer.json").is_file());
    assert!(dest.join("first.php").is_file());
    assert!(!dest.join("second.php").exists(), "must be at the first commit");
    assert_eq!(rev_parse(&dest, "HEAD"), first);
    // origin points back at the real url, not the local mirror.
    let origin = Command::new("git")
        .arg("-C")
        .arg(&dest)
        .args(["remote", "get-url", "origin"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&origin.stdout).trim(), url);
}

#[test]
fn mirror_is_reused_and_dest_is_rewiped() {
    let tmp = TempDir::new().unwrap();
    let (_repo, url, first, second) = fixture_repo(tmp.path());
    let paths = paths_in(tmp.path());

    // First install at the second commit.
    let dest = tmp.path().join("vendor/acme/lib");
    install_source(&paths, &url, &second, &dest).unwrap();
    assert!(dest.join("second.php").is_file());
    // Drop a stray file — a re-install must wipe the tree clean.
    std::fs::write(dest.join("stray.txt"), "x").unwrap();

    // Re-install at the FIRST commit reuses the warm mirror and rewipes.
    install_source(&paths, &url, &first, &dest).unwrap();
    assert_eq!(rev_parse(&dest, "HEAD"), first);
    assert!(!dest.join("stray.txt").exists(), "dest must be rewiped");
    assert!(!dest.join("second.php").exists());

    // The bare mirror was created once and reused.
    let mirrors: Vec<_> = std::fs::read_dir(paths.cache_composer_vcs())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(mirrors.len(), 1, "exactly one cached mirror");
}

/// Downcast a report to the typed VCS error and return its parts.
fn vcs_parts(report: &eyre::Report) -> (String, String) {
    let err = report
        .downcast_ref::<bougie_errors::BougieError>()
        .expect("a BougieError::Vcs");
    match err {
        bougie_errors::BougieError::Vcs { operation, hint, .. } => {
            (operation.clone(), hint.clone())
        }
        other => panic!("expected Vcs, got {other:?}"),
    }
}

#[test]
fn classifier_maps_auth_and_missing_ref_stderr() {
    let auth = classify_git("clone", "https://host/x.git", "remote: Authentication failed for 'x'");
    let (_op, hint) = vcs_parts(&auth);
    assert!(hint.contains("credential") || hint.contains("bougie login"), "{hint}");

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

#[test]
fn install_source_bad_ref_is_typed_ref_error() {
    let tmp = TempDir::new().unwrap();
    let (_repo, url, _first, _second) = fixture_repo(tmp.path());
    let paths = paths_in(tmp.path());
    // A commit that isn't in the repo — checkout must fail with a typed,
    // actionable error rather than a bare git string.
    let bogus = "0000000000000000000000000000000000000000";
    let dest = tmp.path().join("vendor/acme/lib");
    let err = install_source(&paths, &url, bogus, &dest).expect_err("bogus ref must fail");
    let (op, hint) = vcs_parts(&err);
    assert_eq!(op, "checkout");
    assert!(hint.contains("bougie update"), "{hint}");
    assert_eq!(bougie_errors::exit_code_for(&err), 71);
}
