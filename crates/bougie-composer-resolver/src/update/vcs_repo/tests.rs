#![cfg(unix)]
//! Hermetic: build a throwaway git repo (tag + branch) in a tempdir and
//! read it via a `file://` URL — no network.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use bougie_paths::Paths;
use tempfile::TempDir;

use super::read_vcs_packages;
use crate::metadata::VcsRepoConfig;

fn git_ok(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@e", "-c", "user.name=Test", "-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn rev_parse(dir: &Path, rev: &str) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(["rev-parse", rev]).output().unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn paths_in(tmp: &Path) -> Paths {
    Paths::new(tmp.join("home"), tmp.join("cache"))
}

#[test]
fn reads_tag_and_branch_versions_with_source() {
    let tmp = TempDir::new().unwrap();
    let origin = tmp.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git_ok(&origin, &["init", "-q", "-b", "main"]);

    // v1.0.0 tag with a runtime require.
    std::fs::write(
        origin.join("composer.json"),
        r#"{"name":"acme/lib","require":{"psr/log":"^1.0"}}"#,
    )
    .unwrap();
    git_ok(&origin, &["add", "-A"]);
    git_ok(&origin, &["commit", "-q", "-m", "v1"]);
    git_ok(&origin, &["tag", "v1.0.0"]);
    let tag_sha = rev_parse(&origin, "HEAD");

    // A later commit on main, so dev-main resolves to a different sha.
    std::fs::write(origin.join("extra.php"), "<?php\n").unwrap();
    git_ok(&origin, &["add", "-A"]);
    git_ok(&origin, &["commit", "-q", "-m", "wip"]);
    let main_sha = rev_parse(&origin, "HEAD");

    let paths = paths_in(tmp.path());
    let url = format!("file://{}", origin.display());
    let pkgs = read_vcs_packages(&paths, &VcsRepoConfig { url: url.clone() }).unwrap();

    let by_version: HashMap<String, _> =
        pkgs.iter().map(|p| (p.package.version.clone(), p)).collect();

    // Tag → 1.0.0 (leading `v` stripped), source pinned to the tag commit.
    let tag_pkg = by_version.get("1.0.0").expect("tag version present");
    assert_eq!(tag_pkg.package.name, "acme/lib");
    assert!(tag_pkg.package.dist.is_none(), "vcs package has no dist");
    let src = tag_pkg.package.source.as_ref().expect("source block");
    assert_eq!(src.kind, "git");
    assert_eq!(src.url, url);
    assert_eq!(src.reference, tag_sha);
    assert_eq!(tag_pkg.package.require.get("psr/log").map(String::as_str), Some("^1.0"));

    // Branch → dev-main, source pinned to the branch head.
    let main_pkg = by_version.get("dev-main").expect("branch version present");
    assert_eq!(main_pkg.package.source.as_ref().unwrap().reference, main_sha);
}
