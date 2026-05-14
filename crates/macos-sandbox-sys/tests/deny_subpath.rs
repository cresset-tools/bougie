//! A subpath denial must block reads inside the path while leaving
//! peer paths reachable.

#![cfg(target_os = "macos")]

use macos_sandbox_sys::create_sandbox_with_parameters;
use std::{fs::File, io::ErrorKind};

#[test]
fn deny_subpath_blocks_inside_and_allows_outside() {
    let mut src_dir = std::env::current_dir().unwrap();
    src_dir.push("src");
    let res = create_sandbox_with_parameters(
        format!(
            r#"
(version 1)
(allow default)

(deny file-read* file-write*
    (subpath "{}"))"#,
            src_dir.display()
        ),
        0,
        &[],
    );
    assert!(res.is_ok(), "sandbox init failed: {res:?}");

    assert!(File::open("./Cargo.toml").is_ok());

    let err = File::open("./src/lib.rs").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
}
