//! `argv[0]`-dispatched exec path. Bougie is invoked as `php`,
//! `php-fpm`, or `composer` via symlinks under
//! `<project>/.bougie/bin/`. This module recognizes the role and
//! (eventually) execs the right interpreter under the right
//! `PHP_INI_SCAN_DIR`. Phase 2 stubs the body.

use eyre::Result;
use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Php,
    PhpFpm,
    Composer,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Php => "php",
            Self::PhpFpm => "php-fpm",
            Self::Composer => "composer",
        }
    }
}

/// Inspect argv[0]'s basename to decide whether bougie is being
/// invoked as a shim. Returns `None` when the basename is `bougie`
/// (or anything else), meaning the regular CLI dispatch should run.
pub fn role_from_argv0(argv0: &OsStr) -> Option<Role> {
    let stem = Path::new(argv0).file_name()?.to_str()?;
    match stem {
        "php" => Some(Role::Php),
        "php-fpm" => Some(Role::PhpFpm),
        "composer" => Some(Role::Composer),
        _ => None,
    }
}

/// Phase 2 stub. Phase 7 reads `<project>/.bougie/state/resolved` and
/// `execve`s the real interpreter with `PHP_INI_SCAN_DIR` set.
pub fn exec(role: Role) -> Result<ExitCode> {
    eprintln!(
        "{}: project not synced — run `bougie sync` from the project root first.",
        role.name()
    );
    Ok(ExitCode::from(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn detects_each_role_by_basename() {
        assert_eq!(
            role_from_argv0(&OsString::from("/proj/.bougie/bin/php")),
            Some(Role::Php)
        );
        assert_eq!(
            role_from_argv0(&OsString::from("php-fpm")),
            Some(Role::PhpFpm)
        );
        assert_eq!(
            role_from_argv0(&OsString::from("./composer")),
            Some(Role::Composer)
        );
    }

    #[test]
    fn ignores_bougie_basename() {
        assert_eq!(role_from_argv0(&OsString::from("/usr/bin/bougie")), None);
        assert_eq!(role_from_argv0(&OsString::from("bougie")), None);
    }
}
