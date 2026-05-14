//! `(deny default)` profile must accept the syscall — the side-effect
//! denials are exercised by other tests. One-test-per-binary; see
//! `src/lib.rs` for rationale.

#![cfg(target_os = "macos")]

use macos_sandbox_sys::create_sandbox_with_parameters;

#[test]
fn deny_default_profile_initialises() {
    let res =
        create_sandbox_with_parameters("(version 1)\n(deny default)\n".to_string(), 0, &[]);
    assert!(res.is_ok(), "deny-default profile failed: {res:?}");
}
