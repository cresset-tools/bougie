//! Phase 1 smoke tests: the binary parses help and the
//! not-yet-implemented stub exits non-zero.

mod common;

use common::TestEnv;
use predicates::str::contains;

#[test]
fn help_lists_top_level_commands() {
    let env = TestEnv::new();
    env.bougie()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("init"))
        .stdout(contains("ext"))
        .stdout(contains("sync"))
        .stdout(contains("run"))
        .stdout(contains("php"))
        .stdout(contains("cache"))
        .stdout(contains("self"));
}

#[test]
fn version_flag_works() {
    let env = TestEnv::new();
    env.bougie()
        .arg("--version")
        .assert()
        .success()
        .stdout(contains("bougie"));
}

#[test]
fn unimplemented_stub_exits_one() {
    let env = TestEnv::new();
    env.bougie()
        .arg("php")
        .arg("dir")
        .assert()
        .failure()
        .code(1)
        .stderr(contains("not yet implemented"));
}

#[test]
fn unknown_subcommand_errors() {
    let env = TestEnv::new();
    env.bougie()
        .arg("nonsense")
        .assert()
        .failure();
}
