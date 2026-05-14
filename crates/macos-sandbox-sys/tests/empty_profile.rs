//! `(allow default)` profile must let everything through.
//!
//! One-test-per-binary: see the comment at the top of
//! `src/lib.rs` for why `sandbox_init_with_parameters` cannot share a
//! process across multiple tests.

#![cfg(target_os = "macos")]

use macos_sandbox_sys::create_sandbox_with_parameters;
use std::fs::File;

#[test]
fn empty_profile_allows_file_reads() {
    let res =
        create_sandbox_with_parameters("(version 1)\n(allow default)\n".to_string(), 0, &[]);
    assert!(res.is_ok(), "empty profile failed: {res:?}");

    File::open("./Cargo.toml").expect("Cargo.toml should still be readable");
    File::open("./src/lib.rs").expect("src/lib.rs should still be readable");
}
