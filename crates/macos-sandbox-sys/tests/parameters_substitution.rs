//! The `parameters` array must substitute `(param "NAME")` references
//! inside the SBPL profile.

#![cfg(target_os = "macos")]

use macos_sandbox_sys::create_sandbox_with_parameters;
use std::{fs::File, io::ErrorKind};

#[test]
fn parameters_substitution_works() {
    let mut src_dir = std::env::current_dir().unwrap();
    src_dir.push("src");
    let src_str = src_dir.display().to_string();

    let profile = r#"
(version 1)
(allow default)

(deny file-read* file-write*
    (subpath (param "_SUBPATH_DENY")))"#
        .to_string();

    let res = create_sandbox_with_parameters(profile, 0, &["_SUBPATH_DENY", &src_str]);
    assert!(res.is_ok(), "sandbox init failed: {res:?}");

    assert!(File::open("./Cargo.toml").is_ok());

    let err = File::open("./src/lib.rs").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
}
