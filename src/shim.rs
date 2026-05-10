//! `argv[0]`-dispatched exec path. Bougie is invoked as `php`,
//! `php-fpm`, or `composer` via symlinks under
//! `<project>/.bougie/bin/`.

use crate::paths::Paths;
use crate::state::{read_project_resolved, read_project_resolved_composer};
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
    let conf_d = project_root.join(".bougie").join("conf.d");

    match role {
        Role::Php | Role::PhpFpm => {
            let bin = install.join("bin").join(role.name());
            if !bin.exists() {
                return Err(eyre!(
                    "{}: install at {} is missing — re-run `bougie sync`",
                    role.name(),
                    install.display()
                ));
            }
            // execve replaces this process; the only return is an error.
            let err = std::process::Command::new(&bin)
                .args(&args)
                .arg0(&bin)
                .env("PHP_INI_SCAN_DIR", &conf_d)
                .env("PHP_BINARY", &bin)
                .exec();
            Err(err.into())
        }
        Role::Composer => {
            let composer_version =
                read_project_resolved_composer(&project_root).wrap_err_with(|| {
                    format!(
                        "composer: project at {} is not synced — run `bougie sync` first",
                        project_root.display()
                    )
                })?;
            let phar = paths.composer_phar(&composer_version);
            if !phar.exists() {
                return Err(eyre!(
                    "composer: phar at {} is missing — re-run `bougie sync`",
                    phar.display()
                ));
            }
            let php_bin = install.join("bin").join("php");
            if !php_bin.exists() {
                return Err(eyre!(
                    "composer: php at {} is missing — re-run `bougie sync`",
                    php_bin.display()
                ));
            }
            let mut composer_args: Vec<std::ffi::OsString> = Vec::with_capacity(args.len() + 1);
            composer_args.push(phar.into_os_string());
            composer_args.extend(args);
            // PHP_BINARY env var pins the interpreter for child `@php`
            // scripts: without it, Symfony's PhpExecutableFinder falls
            // through to a PATH search and finds the bougie shim.
            let err = std::process::Command::new(&php_bin)
                .args(&composer_args)
                .arg0("composer")
                .env("PHP_INI_SCAN_DIR", &conf_d)
                .env("PHP_BINARY", &php_bin)
                .exec();
            Err(err.into())
        }
    }
}

/// Resolve the project root the shim should read state from. In order:
///
/// 1. `$BOUGIE_PROJECT_ROOT` — set by `bougie run`, the most reliable source.
/// 2. If argv[0] carries a directory part (e.g. `.bougie/bin/php` or an
///    absolute path), walk three parents up from it (`.bougie/bin/php` →
///    `.bougie/bin` → `.bougie` → project root).
/// 3. Otherwise argv[0] is a bare basename (PATH-resolved); walk up from
///    cwd looking for a `.bougie/` directory.
fn locate_project_root(argv0: &OsStr) -> Result<PathBuf> {
    let cwd = std::env::current_dir().wrap_err("getting cwd to locate project root")?;
    locate_project_root_inner(argv0, std::env::var_os("BOUGIE_PROJECT_ROOT"), &cwd)
}

fn locate_project_root_inner(
    argv0: &OsStr,
    env_root: Option<std::ffi::OsString>,
    cwd: &Path,
) -> Result<PathBuf> {
    if let Some(v) = env_root {
        return Ok(PathBuf::from(v));
    }

    let p = Path::new(argv0);
    let has_dir_part = p
        .parent()
        .map_or(false, |q| !q.as_os_str().is_empty());
    if has_dir_part {
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        return abs
            .parent()
            .and_then(|q| q.parent())
            .and_then(|q| q.parent())
            .map(Path::to_path_buf)
            .ok_or_else(|| eyre!("argv[0]={} has no project root", abs.display()));
    }

    for ancestor in cwd.ancestors() {
        if ancestor.join(".bougie").is_dir() {
            return Ok(ancestor.to_path_buf());
        }
    }
    Err(eyre!(
        "no bougie project found (no .bougie/ in {} or any parent)",
        cwd.display()
    ))
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
        let root = locate_project_root_inner(
            OsStr::new("/proj/.bougie/bin/php"),
            None,
            Path::new("/anywhere"),
        )
        .unwrap();
        assert_eq!(root, Path::new("/proj"));
    }

    #[test]
    fn locate_project_root_uses_env_var() {
        let root = locate_project_root_inner(
            OsStr::new("php"),
            Some("/some/proj".into()),
            Path::new("/anywhere"),
        )
        .unwrap();
        assert_eq!(root, Path::new("/some/proj"));
    }

    #[test]
    fn locate_project_root_walks_cwd_for_bare_argv0() {
        let proj = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(proj.path().join(".bougie")).unwrap();
        let sub = proj.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let root = locate_project_root_inner(OsStr::new("php"), None, &sub).unwrap();
        assert_eq!(
            std::fs::canonicalize(&root).unwrap(),
            std::fs::canonicalize(proj.path()).unwrap()
        );
    }

    #[test]
    fn locate_project_root_errors_when_no_marker() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = locate_project_root_inner(OsStr::new("php"), None, dir.path()).unwrap_err();
        assert!(err.to_string().contains("no bougie project"));
    }
}
