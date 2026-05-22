//! Composer's vendored runtime files: copied verbatim from the
//! pinned upstream release into the crate, written into the user's
//! `vendor/composer/` at dump time.
//!
//! The bytes are baked into the binary via `include_bytes!`. Bump
//! [`crate::REFERENCE_COMPOSER_VERSION`] in lockstep with replacing
//! the source files under `vendored/composer-<version>/` and
//! regenerating the byte-equivalence fixtures.
//!
//! `platform_check.php` is conditionally emitted by Composer (only
//! when there's a platform requirement to check) and currently isn't
//! exercised by any fixture, so we don't ship it yet. When that
//! lands it'll get the same treatment.

use std::path::Path;

const VENDORED_DIR: &str = "vendored/composer-2.8.12";

const CLASSLOADER_PHP: &[u8] = include_bytes!("../vendored/composer-2.8.12/ClassLoader.php");
const INSTALLED_VERSIONS_PHP: &[u8] =
    include_bytes!("../vendored/composer-2.8.12/InstalledVersions.php");
const LICENSE: &[u8] = include_bytes!("../vendored/composer-2.8.12/LICENSE");

/// Compile-time pin check: the vendored path under the crate matches
/// the `REFERENCE_COMPOSER_VERSION` constant. Catches the case where
/// somebody bumps the constant without moving the bytes.
const _: () = {
    let pinned = crate::REFERENCE_COMPOSER_VERSION;
    let expected = "2.8.12";
    assert!(str_eq(pinned, expected), "REFERENCE_COMPOSER_VERSION moved away from the vendored bytes; update src/vendored.rs and crates/bougie-autoloader/vendored/composer-<version>/");
    // Keep the path constant honest too — silences dead-code warnings
    // and surfaces the relationship at the call site.
    let _ = VENDORED_DIR;
};

const fn str_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

pub(crate) fn write_runtime_files(
    composer_dir: &Path,
    write: impl Fn(&Path, &[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    write(&composer_dir.join("ClassLoader.php"), CLASSLOADER_PHP)?;
    write(&composer_dir.join("InstalledVersions.php"), INSTALLED_VERSIONS_PHP)?;
    write(&composer_dir.join("LICENSE"), LICENSE)?;
    Ok(())
}
