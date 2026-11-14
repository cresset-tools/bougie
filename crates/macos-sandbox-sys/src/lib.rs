//! FFI bindings for Apple's Sandbox framework. Only compiled on
//! macOS — on every other target this crate is an empty lib so that
//! `cargo {build,test} --workspace` succeeds regardless of host OS.
//! sandbox-run already gates its `macos-sandbox-sys` dep on
//! `cfg(target_os = "macos")`, so nothing here is reachable when the
//! crate compiles to an empty stub.

#![cfg(target_os = "macos")]

use std::{
    ffi::{CStr, CString, c_int},
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

    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// A `*const c_char` that may sit in a `static`. Only ever null.
#[repr(transparent)]
struct NullTerminator(*const c_char);
// SAFETY: the pointer is null and never dereferenced.
unsafe impl Sync for NullTerminator {}

/// An empty, NULL-terminated parameter list.
static NO_PARAMETERS: [NullTerminator; 1] = [NullTerminator(ptr::null())];

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

    // SAFETY: `params_c` is a NULL-terminated array of pointers to C strings
    // that outlive the call.
    unsafe { init(&profile_c, flags, params_c.as_ptr()) }
}

/// Apply `profile` to the calling process, with no parameters.
///
/// Unlike [`create_sandbox_with_parameters`] this does not allocate before the
/// call or when it succeeds, so it suits a `pre_exec` closure: build the
/// `CString` in the parent, call this in the child.
pub fn init_with_profile(profile: &CStr, flags: u64) -> Result<(), String> {
    // SAFETY: `NO_PARAMETERS` is a NULL-terminated array; `NullTerminator` is
    // `repr(transparent)` over one pointer, so the cast yields a valid
    // `*const *const c_char`.
    unsafe { init(profile, flags, NO_PARAMETERS.as_ptr().cast()) }
}

/// # Safety
///
/// `parameters` must point to a NULL-terminated array of valid C strings.
unsafe fn init(profile: &CStr, flags: u64, parameters: *const *const c_char) -> Result<(), String> {
    let mut sandbox_errbuf: *mut c_char = ptr::null_mut();
    let ret = unsafe {
        sandbox_init_with_parameters(profile.as_ptr(), flags, parameters, &mut sandbox_errbuf)
    };

    if ret != 0 {
        if sandbox_errbuf.is_null() {
            return Err("unknown".to_string());
        }
        // libsandbox allocated the buffer, so it is copied out and handed back
        // to libsandbox. Taking ownership with `CString::from_raw` would free
        // it through Rust's allocator, which did not allocate it.
        let error = unsafe { CStr::from_ptr(sandbox_errbuf) }
            .to_string_lossy()
            .into_owned();
        unsafe { sandbox_free_error(sandbox_errbuf) };
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
