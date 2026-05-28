//! Integration tests for `bougie tool` surfaces that don't need a
//! real PHP install or a real composer round-trip:
//!
//! - `bougie tool list` against a hand-built tool dir,
//! - `bougie tool dir [pkg]`,
//! - `bougie tool uninstall` cleaning up symlinks + the tool dir,
//! - `bougie tool-exec` rejecting paths outside `$BOUGIE_LOCAL/tools/`,
//! - `bougie tool-exec` surfacing the "receipt missing" recovery hint.
//!
//! A full install→exec round-trip needs an installed PHP plus network
//! access to Packagist; that's deferred to a separate fixture-driven
//! test once a stable fixture PHP shape lands.

mod common;

use common::TestEnv;
use predicates::str::contains;
use std::path::{Path, PathBuf};

fn make_tool_dir(home: &Path, package: &str, php_bin: &Path) -> PathBuf {
    let bin_name = package.rsplit_once('/').unwrap().1;
    make_tool_dir_with_bins(home, package, php_bin, &[bin_name])
}

/// Multi-bin variant: writes one wrapper + one entrypoint per name in
/// `bin_names`. Symlinks land in `<home>/.local/bin/<name>` (which the
/// test then overrides via `BOUGIE_TOOL_BIN_DIR` if relevant).
fn make_tool_dir_with_bins(
    home: &Path,
    package: &str,
    php_bin: &Path,
    bin_names: &[&str],
) -> PathBuf {
    let tool_dir = home.join("tools").join(package.replace('/', "-"));
    std::fs::create_dir_all(tool_dir.join("bin")).unwrap();
    std::fs::create_dir_all(tool_dir.join("conf.d")).unwrap();

    let mut entry_blocks = String::new();
    for name in bin_names {
        let wrapper = tool_dir.join("bin").join(name);
        std::fs::write(&wrapper, "<?php\n").unwrap();
        entry_blocks.push_str(&format!(
            "\n[[entrypoints]]\n\
             name = \"{name}\"\n\
             install_path = \"{install}\"\n\
             from = \"{package}\"\n",
            install = wrapper.display(),
        ));
    }

    let receipt = format!(
        "package = \"{package}\"\n\
         constraint = \"^1.10\"\n\
         php_version = \"8.3.12\"\n\
         php_flavor = \"nts\"\n\
         composer_version = \"2.8.12\"\n\
         with = []\n\
         php_resolved_path = \"{php}\"\n\
         {entry_blocks}",
        php = php_bin.display(),
    );
    std::fs::write(tool_dir.join("receipt.toml"), receipt).unwrap();
    tool_dir
}

#[test]
fn tool_dir_with_no_package_prints_tools_root() {
    let env = TestEnv::new();
    env.bougie()
        .args(["tool", "dir"])
        .assert()
        .success()
        .stdout(contains("tools"));
}

#[test]
fn tool_dir_with_package_prints_per_tool_path() {
    let env = TestEnv::new();
    env.bougie()
        .args(["tool", "dir", "phpstan/phpstan"])
        .assert()
        .success()
        .stdout(contains("tools/phpstan-phpstan"));
}

#[test]
fn tool_dir_rejects_bare_package_name() {
    let env = TestEnv::new();
    env.bougie()
        .args(["tool", "dir", "phpstan"])
        .assert()
        .failure()
        .stderr(contains("missing the vendor"));
}

#[test]
fn tool_list_empty_when_no_tools_installed() {
    let env = TestEnv::new();
    env.bougie()
        .args(["tool", "list"])
        .assert()
        .success()
        .stdout(contains("no tools installed"));
}

#[test]
fn tool_list_shows_healthy_tool() {
    let env = TestEnv::new();
    // Pretend PHP is the test binary so `php_resolved_path.exists()`.
    let fake_php = std::env::current_exe().unwrap();
    make_tool_dir(env.home_path(), "phpstan/phpstan", &fake_php);
    env.bougie()
        .args(["tool", "list"])
        .assert()
        .success()
        .stdout(contains("phpstan/phpstan"))
        .stdout(contains("php 8.3.12"));
}

#[test]
fn tool_list_marks_stale_tool_when_php_missing() {
    let env = TestEnv::new();
    let fake_php = env.home_path().join("nope").join("php");
    make_tool_dir(env.home_path(), "phpstan/phpstan", &fake_php);
    env.bougie()
        .args(["tool", "list"])
        .assert()
        .success()
        .stdout(contains("STALE"));
}

#[test]
fn tool_list_marks_broken_dir_without_receipt() {
    let env = TestEnv::new();
    let tool_dir = env.home_path().join("tools").join("phpstan-phpstan");
    std::fs::create_dir_all(&tool_dir).unwrap();
    env.bougie()
        .args(["tool", "list"])
        .assert()
        .success()
        .stdout(contains("BROKEN"))
        .stdout(contains("receipt.toml missing"));
}

#[test]
fn tool_uninstall_removes_dir_and_symlinked_bin() {
    let env = TestEnv::new();
    let fake_php = std::env::current_exe().unwrap();
    let tool_dir = make_tool_dir(env.home_path(), "phpstan/phpstan", &fake_php);

    // Drop a sentinel "bin" file at the path the receipt records, so
    // uninstall actually has something to delete.
    let install_path = tool_dir.join("bin").join("phpstan");
    assert!(install_path.exists());

    env.bougie()
        .args(["tool", "uninstall", "phpstan/phpstan"])
        .assert()
        .success()
        .stdout(contains("uninstalled phpstan/phpstan"));

    assert!(!tool_dir.exists(), "tool dir should be gone");
}

#[test]
fn tool_list_shows_multi_bin_tool() {
    let env = TestEnv::new();
    let fake_php = std::env::current_exe().unwrap();
    make_tool_dir_with_bins(
        env.home_path(),
        "vimeo/psalm",
        &fake_php,
        &["psalm", "psalter"],
    );
    env.bougie()
        .args(["tool", "list"])
        .assert()
        .success()
        .stdout(contains("vimeo/psalm"))
        .stdout(contains("psalm, psalter"));
}

#[test]
fn tool_uninstall_removes_all_bins_for_multi_bin_tool() {
    let env = TestEnv::new();
    let fake_php = std::env::current_exe().unwrap();
    let tool_dir = make_tool_dir_with_bins(
        env.home_path(),
        "vimeo/psalm",
        &fake_php,
        &["psalm", "psalter"],
    );
    let bin_a = tool_dir.join("bin").join("psalm");
    let bin_b = tool_dir.join("bin").join("psalter");
    assert!(bin_a.exists());
    assert!(bin_b.exists());

    env.bougie()
        .args(["tool", "uninstall", "vimeo/psalm"])
        .assert()
        .success();

    assert!(!tool_dir.exists(), "multi-bin tool dir should be gone");
}

#[test]
fn tool_uninstall_errors_for_unknown_tool() {
    let env = TestEnv::new();
    env.bougie()
        .args(["tool", "uninstall", "phpstan/phpstan"])
        .assert()
        .failure()
        .stderr(contains("not installed"));
}

#[test]
fn tool_inject_errors_for_unknown_tool() {
    let env = TestEnv::new();
    env.bougie()
        .args(["tool", "inject", "phpstan/phpstan", "--with", "vendor/extra"])
        .assert()
        .failure()
        .stderr(contains("not installed"));
}

#[test]
fn tool_uninject_errors_when_extra_absent() {
    let env = TestEnv::new();
    let fake_php = std::env::current_exe().unwrap();
    make_tool_dir(env.home_path(), "phpstan/phpstan", &fake_php);
    env.bougie()
        .args([
            "tool",
            "uninject",
            "phpstan/phpstan",
            "--with",
            "phpstan/phpstan-strict-rules",
        ])
        .assert()
        .failure()
        .stderr(contains("not currently injected"));
}

#[test]
fn tool_inject_rejects_bare_name_with_classifier_hint() {
    let env = TestEnv::new();
    let fake_php = std::env::current_exe().unwrap();
    make_tool_dir(env.home_path(), "phpstan/phpstan", &fake_php);
    env.bougie()
        .args(["tool", "inject", "phpstan/phpstan", "--with", "intl"])
        .assert()
        .failure()
        .stderr(contains("isn't a known PHP extension"));
}

#[test]
fn tool_exec_rejects_wrapper_outside_tools_dir() {
    let env = TestEnv::new();
    // Ensure the tools dir exists (canonicalize would otherwise fail
    // on the `tools_root` side and we'd mistake that for the intended
    // rejection).
    std::fs::create_dir_all(env.home_path().join("tools")).unwrap();
    let stray = env.home_path().join("elsewhere").join("phpstan");
    std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
    std::fs::write(&stray, "<?php\n").unwrap();
    env.bougie()
        .args(["tool-exec".as_ref(), stray.as_os_str()])
        .assert()
        .failure()
        .stderr(contains("not under"));
}

#[test]
fn tool_exec_surfaces_missing_receipt_with_recovery_hint() {
    let env = TestEnv::new();
    let tool_dir = env.home_path().join("tools").join("phpstan-phpstan");
    std::fs::create_dir_all(tool_dir.join("bin")).unwrap();
    let wrapper = tool_dir.join("bin").join("phpstan");
    std::fs::write(&wrapper, "<?php\n").unwrap();
    // No receipt.toml.
    env.bougie()
        .args(["tool-exec".as_ref(), wrapper.as_os_str()])
        .assert()
        .failure()
        .stderr(contains("receipt.toml missing"))
        .stderr(contains("--reinstall"));
}
