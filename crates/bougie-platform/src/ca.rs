//! CA certificates for bougie-managed PHP.
//!
//! bougie's PHP links its own OpenSSL, built with `--openssldir=/etc/ssl`,
//! and its libcurl defers to OpenSSL's defaults (`--with-ca-fallback`). So
//! PHP trusts `/etc/ssl/cert.pem` plus the hashed `/etc/ssl/certs/`
//! directory. Every mainstream distro provides at least one of those, but
//! NixOS provides neither: its only bundle is
//! `/etc/ssl/certs/ca-certificates.crt` (what nixpkgs' own OpenSSL is
//! patched to use), so HTTPS from PHP failed certificate verification.
//!
//! When OpenSSL's default file is missing, point `SSL_CERT_FILE` at that
//! bundle. OpenSSL then loads it in addition to the hashed directory, and
//! both PHP streams and curl pick it up. Only current locations are
//! checked; legacy compatibility paths are deliberately left out.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The environment variable OpenSSL reads for its default CA file.
pub const SSL_CERT_FILE: &str = "SSL_CERT_FILE";

/// OpenSSL's default CA file for bougie's build (`OPENSSLDIR/cert.pem`).
const OPENSSL_DEFAULT_CERT_FILE: &str = "/etc/ssl/cert.pem";

/// The system bundle on NixOS (and Debian, Ubuntu, Arch and Gentoo).
const SYSTEM_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";

/// The `SSL_CERT_FILE` value to give a bougie-managed PHP, or `None` to
/// leave the environment as it is.
pub fn ssl_cert_file() -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    choose(
        std::env::var_os(SSL_CERT_FILE),
        std::env::var_os("SSL_CERT_DIR"),
        Path::is_file,
    )
}

fn choose(
    cert_file: Option<OsString>,
    cert_dir: Option<OsString>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    // The user already chose where OpenSSL looks.
    if cert_file.is_some_and(|v| !v.is_empty()) || cert_dir.is_some_and(|v| !v.is_empty()) {
        return None;
    }
    if is_file(Path::new(OPENSSL_DEFAULT_CERT_FILE)) {
        return None;
    }
    let bundle = Path::new(SYSTEM_BUNDLE);
    is_file(bundle).then(|| bundle.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present(paths: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |p| paths.iter().any(|q| Path::new(q) == p)
    }

    #[test]
    fn nixos_gets_the_system_bundle() {
        assert_eq!(
            choose(None, None, present(&[SYSTEM_BUNDLE])),
            Some(PathBuf::from(SYSTEM_BUNDLE))
        );
    }

    #[test]
    fn openssl_default_present_leaves_env_alone() {
        // Fedora, RHEL, Arch, Alpine: cert.pem exists.
        assert_eq!(
            choose(
                None,
                None,
                present(&[OPENSSL_DEFAULT_CERT_FILE, SYSTEM_BUNDLE])
            ),
            None
        );
    }

    #[test]
    fn user_setting_wins() {
        let all = present(&[SYSTEM_BUNDLE]);
        assert_eq!(choose(Some("/my/ca.pem".into()), None, &all), None);
        assert_eq!(choose(None, Some("/my/certs".into()), &all), None);
    }

    #[test]
    fn empty_user_setting_is_ignored() {
        assert_eq!(
            choose(Some(OsString::new()), None, present(&[SYSTEM_BUNDLE])),
            Some(PathBuf::from(SYSTEM_BUNDLE))
        );
    }

    #[test]
    fn no_bundle_leaves_env_alone() {
        // openSUSE: only the hashed directory, which OpenSSL already uses.
        assert_eq!(
            choose(None, None, present(&["/etc/ssl/ca-bundle.pem"])),
            None
        );
    }
}
