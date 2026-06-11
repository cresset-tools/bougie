//! Phase 5 regression: `bougie composer <unmapped>` no longer forwards
//! to a Composer phar — bougie does not bundle one. Unmapped
//! subcommands must error with a pointer to `bougie tool install
//! composer/composer`, and native subcommands must still dispatch.

use tempfile::TempDir;

mod common;
use common::TestEnv;

#[test]
fn unmapped_composer_subcommand_errors_with_tool_hint() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    std::fs::write(proj.path().join("composer.json"), r#"{"name":"t/p"}"#).unwrap();

    let out = env
        .bougie()
        .args(["composer", "create-project", "acme/skel", "dir", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();

    assert!(!out.status.success(), "create-project must not succeed (no phar)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bougie tool install composer/composer"),
        "error should point at the tool escape hatch: {stderr}"
    );
    // Crucially, it must NOT have tried to fetch/run a phar.
    assert!(
        !stderr.contains("phar at") && !stderr.contains("run `bougie sync`"),
        "must not reference a phar: {stderr}"
    );
}

#[test]
fn native_composer_subcommand_still_dispatches() {
    // `validate` is native; it should run (and fail cleanly on a
    // bogus composer.json) rather than hit any phar path.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"name":"test/proj","require":{}}"#,
    )
    .unwrap();

    let out = env
        .bougie()
        .args(["composer", "validate", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    // Either success or a validation error — the point is it dispatched
    // natively (no "phar"/"sync" message).
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!combined.contains("phar"), "validate must be native: {combined}");
}
