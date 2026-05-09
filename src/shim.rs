//! `argv[0]`-dispatched exec path. Bougie is invoked as `php`,
//! `php-fpm`, or `composer` via symlinks under
//! `<project>/.bougie/bin/`.

use crate::paths::Paths;
use crate::state::read_project_resolved;
use eyre::{eyre, Result, WrapErr};
use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
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

pub fn role_from_argv0(argv0: &OsStr) -> Option<Role> {
    let stem = Path::new(argv0).file_name()?.to_str()?;
    match stem {
        "php" => Some(Role::Php),
        "php-fpm" => Some(Role::PhpFpm),
        "composer" => Some(Role::Composer),
        _ => None,
    }
}

/// Read `<project>/.bougie/state/resolved`, locate the install in
/// `$BOUGIE_HOME/installs/<resolved>/`, set `PHP_INI_SCAN_DIR`, and
/// `execve` the real interpreter (or `composer`).
pub fn exec(role: Role) -> Result<ExitCode> {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.is_empty() {
        return Err(eyre!("missing argv[0]"));
    }
    let argv0 = args.remove(0);
    let project_root = locate_project_root(&argv0)?;

    let (version, flavor) = read_project_resolved(&project_root).wrap_err_with(|| {
        format!(
            "{}: project at {} is not synced — run `bougie sync` first",
            role.name(),
            project_root.display()
        )
    })?;

    let paths = Paths::from_env()?;
    let install = paths.installs().join(format!("{version}-{flavor}"));
    let bin = install.join("bin").join(role.name());
    if !bin.exists() {
        return Err(eyre!(
            "{}: install at {} is missing — re-run `bougie sync`",
            role.name(),
            install.display()
        ));
    }

    let conf_d = project_root.join(".bougie").join("conf.d");

    // execve replaces this process; the only return path is an error.
    let err = std::process::Command::new(&bin)
        .args(&args)
        .arg0(&bin)
        .env("PHP_INI_SCAN_DIR", &conf_d)
        .exec();
    Err(err.into())
}

/// argv[0] is the symlink path (e.g. `.bougie/bin/php`); two `parent()`
/// calls take us to the project root.
fn locate_project_root(argv0: &OsStr) -> Result<PathBuf> {
    let p = Path::new(argv0);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .wrap_err("getting cwd to resolve relative argv[0]")?
            .join(p)
    };
    abs.parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .ok_or_else(|| eyre!("argv[0]={} has no project root", abs.display()))
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

    #[test]
    fn locate_project_root_walks_two_parents() {
        let root = locate_project_root(OsStr::new("/proj/.bougie/bin/php")).unwrap();
        assert_eq!(root, Path::new("/proj"));
    }
}
