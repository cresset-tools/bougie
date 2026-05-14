//! FFI bindings for Apple's Sandbox framework. Only compiled on
//! macOS — on every other target this crate is an empty lib so that
//! `cargo {build,test} --workspace` succeeds regardless of host OS.
//! sandbox-run already gates its `macos-sandbox-sys` dep on
//! `cfg(target_os = "macos")`, so nothing here is reachable when the
//! crate compiles to an empty stub.

#![cfg(target_os = "macos")]

use std::{
    ffi::{CString, c_int},
    os::raw::c_char,
    ptr,
};

#[link(name = "sandbox")]
unsafe extern "C" {
    fn sandbox_init_with_parameters(
        profile: *const c_char,
        flags: u64,
        parameters: *const *const c_char,
        errorbuf: *mut *mut c_char,
    ) -> c_int;
}

pub fn create_sandbox_with_parameters(
    profile: String,
    flags: u64,
    parameters: &[&str],
) -> Result<(), String> {
    // Unwrap: safe because a Rust String cannot contain null bytes
    let profile_c = CString::new(profile).unwrap();
    let parameters_cstrings = strings_to_cstrings(parameters);

    let mut params_c: Vec<*const c_char> = parameters_cstrings.iter().map(|x| x.as_ptr()).collect();
    params_c.push(ptr::null());

    let mut sandbox_errbuf: *mut c_char = ptr::null_mut();
    let ret = unsafe {
        sandbox_init_with_parameters(
            profile_c.as_ptr(),
            flags,
            params_c.as_ptr(),
            &mut sandbox_errbuf,
        )
    };

    if ret != 0 {
        let error = if sandbox_errbuf.is_null() {
            "unknown".to_string()
        } else {
            unsafe { CString::from_raw(sandbox_errbuf) }
                .into_string()
                .unwrap()
        };
        return Err(error);
    }
    Ok(())
}

fn strings_to_cstrings(strings: &[&str]) -> Vec<CString> {
    // Unwrap: safe because a Rust String cannot contain null bytes
    strings
        .iter()
        .map(|x| CString::new(*x))
        .map(Result::unwrap)
        .collect()
}

#[cfg(test)]
mod test {
    use std::{fs::File, io::ErrorKind};

    use crate::create_sandbox_with_parameters;

    #[test]
    fn test_empty_profile() {
        let res =
            create_sandbox_with_parameters("(version 1)\n(allow default)\n".to_string(), 0, &[]);
        assert!(res.is_ok());

        // try to read file
        let _file = File::open("./Cargo.toml").unwrap();

        let _file = File::open("./src/lib.rs").unwrap();
    }

    #[test]
    fn test_basic_deny_profile() {
        let res =
            create_sandbox_with_parameters("(version 1)\n(deny default)\n".to_string(), 0, &[]);
        assert!(res.is_ok());
    }

    #[test]
    fn test_deny_subpath() {
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
        assert!(res.is_ok());

        // try to read file
        assert!(File::open("./Cargo.toml").is_ok());

        let err = File::open("./src/lib.rs").unwrap_err();
        assert!(err.kind() == ErrorKind::PermissionDenied);
    }

    #[test]
    fn test_parameters_substitution() {
        let mut src_dir = std::env::current_dir().unwrap();
        src_dir.push("src");
        let src_str = src_dir.display().to_string();

        let profile = format!(
            r#"
(version 1)
(allow default)

(deny file-read* file-write*
    (subpath (param "_SUBPATH_DENY")))"#
        );

        let res = create_sandbox_with_parameters(profile, 0, &["_SUBPATH_DENY", &src_str]);
        assert!(res.is_ok());

        // try to read a file outside the denied path
        assert!(File::open("./Cargo.toml").is_ok());

        // access inside the denied path should be denied
        let err = File::open("./src/lib.rs").unwrap_err();
        assert!(err.kind() == ErrorKind::PermissionDenied);
    }
}
