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

// Tests live under `tests/` rather than as a `#[cfg(test)] mod test`
// inside this file. Apple's `sandbox_init_with_parameters` is a
// process-once API: the first call applies, every subsequent call in
// the same process returns EPERM. The lib's test runner runs all
// `#[test]`s inside the same binary, so a `mod tests` here would only
// ever see one test pass and the others fail at random depending on
// thread scheduling. Cargo's integration tests get one binary per
// `tests/*.rs` file, which gives each test its own process — the
// only sound isolation for an API like this.
