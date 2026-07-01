//! `argv[0]`-dispatched exec path. Bougie is invoked as `php`,
//! `php-fpm`, `composer`, or `unzip` via symlinks under
//! `<project>/vendor/bougie/bin/`. The `unzip` role exists because Composer's
//! `ZipDownloader` prefers a PATH `unzip` over PHP's `ZipArchive`; see
//! `commands::unzip` for the invocation surface.
//!
//! `bougied` is also a role on the same binary: when invoked under
//! `argv[0] == "bougied"`, the process becomes the long-lived service
//! supervisor daemon. The CLI auto-spawns it on first
//! `bougie services …` invocation by exec'ing `current_exe()` with the
//! `bougied` argv[0] override.

use crate::commands::unzip;
use bougie_paths::Paths;
use bougie_fs::state::read_project_resolved;
use eyre::{eyre, Result, WrapErr};
use std::ffi::OsStr;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Php,
    PhpFpm,
    Composer,
    Unzip,
    /// `bougie-run <script> [args]` — the `#!/usr/bin/env bougie-run`
    /// shebang fallback for systems whose `env` lacks `-S` (so the
    /// portable `#!/usr/bin/env -S bougie run --script` can't be used).
    /// Routes straight to `bougie run --script <script> [args]`.
    ScriptRun,
    #[cfg(unix)]
    Bougied,
    #[cfg(unix)]
    Babysit,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Php => "php",
            Self::PhpFpm => "php-fpm",
            Self::Composer => "composer",
            Self::Unzip => "unzip",
            Self::ScriptRun => "bougie-run",
            #[cfg(unix)]
            Self::Bougied => "bougied",
            #[cfg(unix)]
            Self::Babysit => "bougie-babysit",
        }
    }
}

pub fn role_from_argv0(argv0: &OsStr) -> Option<Role> {
    let stem = Path::new(argv0).file_name()?.to_str()?;
    // On Windows the symlink/hardlink shims carry the `.exe` suffix
    // (see `commands::sync::write_shims`); strip it so the basename
    // comparison matches the same role names as on Unix. Windows
    // file names are case-insensitive — Composer's `ZipDownloader`
    // surfaces the invocation as `unzip.EXE` (capitalised) on some
    // PHP paths, so accept either casing or the bougie shim falls
    // through to the global clap CLI parser ("argument '--quiet'
    // cannot be used multiple times" for `unzip -qq`).
    let stem = if stem.len() >= 4 && stem[stem.len() - 4..].eq_ignore_ascii_case(".exe") {
        &stem[..stem.len() - 4]
    } else {
        stem
    };
    match stem {
        "php" => Some(Role::Php),
        "php-fpm" => Some(Role::PhpFpm),
        "composer" => Some(Role::Composer),
        "unzip" => Some(Role::Unzip),
        "bougie-run" => Some(Role::ScriptRun),
        #[cfg(unix)]
        "bougied" => Some(Role::Bougied),
        #[cfg(unix)]
        "bougie-babysit" => Some(Role::Babysit),
        _ => None,
    }
}

/// Ini overrides bougie injects ahead of the caller's args, for the
/// **CLI** `php` role only (never `php-fpm`, which keeps the server's
/// configured limits). Today: lift the memory limit — Magento's
/// `bin/magento` and Composer routinely blow past php.ini's 128M default,
/// and `memory_limit=-1` is Magento's own CLI recommendation. Prepended,
/// so a caller's explicit `-d memory_limit=…` overrides it (PHP uses the
/// last value given for a directive).
fn cli_php_prelude_args(role: Role) -> &'static [&'static str] {
    match role {
        Role::Php => &["-d", "memory_limit=-1"],
        _ => &[],
    }
}

/// Read `<project>/vendor/bougie/state/resolved`, locate the install in
/// `$BOUGIE_HOME/installs/<resolved>/`, set `PHP_INI_SCAN_DIR`, and
/// `execve` the real interpreter (or `composer`).
pub fn exec(role: Role) -> Result<ExitCode> {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.is_empty() {
        return Err(eyre!("missing argv[0]"));
    }
    let argv0 = args.remove(0);

    // The unzip role is project-agnostic — Composer's `ZipDownloader`
    // calls `unzip` from its own working directory and only cares about
    // archive extraction. Skip project resolution entirely.
    if role == Role::Unzip {
        return unzip::run(args);
    }

    // The bougied role is also project-agnostic: it's a per-user
    // long-lived supervisor, not bound to any single project. The CLI
    // auto-spawns it via `current_exe()` with argv[0] = "bougied".
    #[cfg(unix)]
    if role == Role::Bougied {
        let paths = Paths::from_env()?;
        return bougie_daemon::daemon::run(paths);
    }

    // The babysit role is the per-service supervisor shim that
    // bougied spawns to own a service's process group and clean it
    // up on parent death. Project-agnostic.
    #[cfg(unix)]
    if role == Role::Babysit {
        return bougie_babysit::run(args);
    }

    // The `composer` shim routes to bougie's *native* Composer
    // subcommands — there is no Composer phar. It's project-resolved
    // from the working directory by the native commands themselves, so
    // (like the project-agnostic roles above) it skips the shared
    // "must be synced" resolution below.
    if role == Role::Composer {
        return run_composer_native(args);
    }

    // The `bougie-run` shebang shim is project-agnostic: a self-contained
    // script carries its own deps, so there's no surrounding project to
    // resolve. `args` is already `[<script>, <script-args>…]` — exactly
    // what `commands::script::run` expects as its argv.
    if role == Role::ScriptRun {
        return run_script_shim(args);
    }

    let project_root = locate_project_root(&argv0)?;

    // System PHP: a `resolved-php-path` marker means sync selected a
    // system interpreter rather than a bougie-managed install. Exec it
    // directly — there is no install tree, and bougie deliberately does
    // not set PHP_INI_SCAN_DIR (it can't load its ABI-controlled
    // extensions onto a foreign build, so the system PHP keeps its own
    // conf.d intact).
    if let Some(system_php) = bougie_fs::state::read_project_resolved_php_path(&project_root) {
        return exec_system_php(role, &system_php, args);
    }

    let (version, flavor) = read_project_resolved(&project_root).wrap_err_with(|| {
        format!(
            "{}: project at {} is not synced — run `bougie sync` first",
            role.name(),
            project_root.display()
        )
    })?;

    let paths = Paths::from_env()?;
    let install = paths.installs().join(format!("{version}-{flavor}"));
    // Honor an active xdebug session signalled by the parent's
    // XDEBUG_SESSION env var: include the project's debug overlay
    // dir in PHP_INI_SCAN_DIR so xdebug.so loads for this exec.
    // `bougie run --xdebug` exports XDEBUG_SESSION=1 specifically so
    // this branch fires here in the shim — see
    // `commands::run::run`.
    let debug_overlay = bougie_installer::conf_d::xdebug_session_env_active();
    let scan_dir = bougie_installer::conf_d::php_ini_scan_dir(&paths, &project_root, debug_overlay);

    match role {
        Role::Php | Role::PhpFpm => {
            // On Windows, the per-version PHP install ships `php.exe` /
            // `php-fpm.exe`; on Unix it's bare `php` / `php-fpm`.
            let bin = install.join("bin").join(exe_name(role.name()));
            if !bin.exists() {
                return Err(eyre!(
                    "{}: install at {} is missing — re-run `bougie sync`",
                    role.name(),
                    install.display()
                ));
            }
            let mut cmd = std::process::Command::new(&bin);
            // CLI-only ini defaults (never FPM) — prepended so a caller's
            // explicit `-d …` still wins (PHP takes the last value for a
            // directive).
            cmd.args(cli_php_prelude_args(role));
            cmd.args(&args)
                .env("PHP_INI_SCAN_DIR", &scan_dir)
                .env("PHP_BINARY", &bin);
            #[cfg(unix)]
            cmd.arg0(&bin);
            run_and_propagate(cmd, role.name())
        }
        Role::Composer => unreachable!("composer role handled above"),
        Role::Unzip => unreachable!("unzip role handled above"),
        Role::ScriptRun => unreachable!("bougie-run role handled above"),
        #[cfg(unix)]
        Role::Bougied => unreachable!("bougied role handled above"),
        #[cfg(unix)]
        Role::Babysit => unreachable!("babysit role handled above"),
    }
}

/// Exec a **system** PHP for the `php` / `php-fpm` role. `system_php`
/// is the absolute path to the system `php` binary recorded in
/// `resolved-php-path`; the `php-fpm` role resolves a sibling `php-fpm`
/// next to it. No `PHP_INI_SCAN_DIR` is set (see [`exec`]).
fn exec_system_php(
    role: Role,
    system_php: &Path,
    args: Vec<std::ffi::OsString>,
) -> Result<ExitCode> {
    let bin = match role {
        Role::Php => system_php.to_path_buf(),
        Role::PhpFpm => bougie_fs::state::system_fpm_for_php(system_php).ok_or_else(|| {
            eyre!(
                "php-fpm: system PHP at {} has no php-fpm alongside it \
                 (looked in its bin/ and ../sbin/); server features need a \
                 managed PHP (drop `--no-managed-php`)",
                system_php.display()
            )
        })?,
        _ => unreachable!("exec_system_php only handles php / php-fpm"),
    };
    if !bin.exists() {
        return Err(eyre!(
            "{}: system PHP at {} no longer exists — re-run `bougie sync`",
            role.name(),
            bin.display()
        ));
    }
    let mut cmd = std::process::Command::new(&bin);
    cmd.args(cli_php_prelude_args(role));
    cmd.args(&args).env("PHP_BINARY", &bin);
    #[cfg(unix)]
    cmd.arg0(&bin);
    run_and_propagate(cmd, role.name())
}

/// The `composer` argv[0] shim routes to bougie's **native** Composer
/// subcommands — bougie does not bundle or execute the Composer phar.
///
/// `args` is the raw composer argument vector (e.g. `["install",
/// "--no-dev"]`); it's re-parsed as `bougie composer <args>` and
/// dispatched through the normal CLI. Native subcommands
/// (`install`/`update`/`require`/`show`/…) run natively; an unrecognized
/// subcommand routes to `ComposerCommand::External`, which returns the
/// "install the real Composer via `bougie tool`" error.
///
/// Shared by the project-local `vendor/bougie/bin/composer` shim and the
/// global `composer` entry seeded into the tool bin dir, so a bare
/// `composer …` from any shell behaves exactly like `bougie composer …`.
fn run_composer_native(args: Vec<std::ffi::OsString>) -> Result<ExitCode> {
    use clap::Parser as _;
    let mut argv: Vec<std::ffi::OsString> = Vec::with_capacity(args.len() + 2);
    argv.push(std::ffi::OsString::from("bougie"));
    argv.push(std::ffi::OsString::from("composer"));
    argv.extend(args);
    match crate::Cli::try_parse_from(argv) {
        Ok(cli) => crate::run(cli),
        // Usage error, `--help`, or `--version`: clap already formatted
        // the message and chose the stream; mirror its exit convention.
        Err(e) => {
            let _ = e.print();
            Ok(ExitCode::from(u8::from(e.use_stderr()) * 2))
        }
    }
}

/// The `bougie-run` argv[0] shim: route `bougie-run <script> [args]`
/// straight to `commands::script::run` (i.e. `bougie run --script …`).
/// `args` is the post-argv[0] vector — already `[<script>, <args>…]`,
/// the shape `script::run` consumes. Only the bare form is available via
/// the shebang (no `--php` / `--with` / `--xdebug`); use the explicit
/// `bougie run --script` for those.
fn run_script_shim(raw_args: Vec<std::ffi::OsString>) -> Result<ExitCode> {
    let argv: Vec<String> = raw_args
        .into_iter()
        .map(|a| {
            a.into_string()
                .map_err(|bad| eyre!("script argument is not valid UTF-8: {bad:?}"))
        })
        .collect::<Result<_>>()?;
    crate::commands::script::run(
        &argv,
        bougie_cli::OutputFormat::Text,
        None,
        &[],
        false,
        bougie_cli::PhpPrefArgs::default(),
    )
}

/// On Unix, append the target executable name verbatim (`php`).
/// On Windows, append `.exe` so the file actually exists on disk.
fn exe_name(stem: &str) -> String {
    #[cfg(unix)]
    {
        stem.to_owned()
    }
    #[cfg(not(unix))]
    {
        format!("{stem}.exe")
    }
}

/// On Unix, replace the current process with `cmd` via `execve`. The
/// only return is an error.
/// On Windows there is no execve; spawn the child, wait, and propagate
/// its exit code.
fn run_and_propagate(mut cmd: std::process::Command, label: &str) -> Result<ExitCode> {
    #[cfg(unix)]
    {
        let err = cmd.exec();
        Err(err).wrap_err_with(|| format!("exec {label}"))
    }
    #[cfg(not(unix))]
    {
        let status = cmd
            .status()
            .wrap_err_with(|| format!("spawning {label}"))?;
        let code = status.code().unwrap_or(1);
        let code = u8::try_from(code).unwrap_or(1);
        Ok(ExitCode::from(code))
    }
}

/// Resolve the project root the shim should read state from. In order:
///
/// 1. `$BOUGIE_PROJECT_ROOT` — set by `bougie run`, the most reliable source.
/// 2. If argv[0] carries a directory part (e.g. `vendor/bougie/bin/php`
///    or an absolute path), walk four parents up from it
///    (`vendor/bougie/bin/php` → `vendor/bougie/bin` → `vendor/bougie`
///    → `vendor` → project root).
/// 3. Otherwise argv[0] is a bare basename (PATH-resolved); walk up from
///    cwd looking for a `vendor/bougie/` directory.
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
        .is_some_and(|q| !q.as_os_str().is_empty());
    if has_dir_part {
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        // `vendor/bougie/bin/php` → `vendor/bougie/bin` →
        // `vendor/bougie` → `vendor` → project root (four parents).
        return abs
            .parent()
            .and_then(|q| q.parent())
            .and_then(|q| q.parent())
            .and_then(|q| q.parent())
            .map(Path::to_path_buf)
            .ok_or_else(|| eyre!("argv[0]={} has no project root", abs.display()));
    }

    for ancestor in cwd.ancestors() {
        if bougie_paths::project::is_root(ancestor) {
            return Ok(ancestor.to_path_buf());
        }
    }
    Err(eyre!(
        "no bougie project found (no vendor/bougie/ in {} or any parent)",
        cwd.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn cli_php_lifts_memory_limit_but_not_fpm() {
        // CLI php gets an unlimited memory_limit prepended; FPM gets
        // nothing (keeps the server's configured limit).
        assert_eq!(cli_php_prelude_args(Role::Php), &["-d", "memory_limit=-1"]);
        assert!(cli_php_prelude_args(Role::PhpFpm).is_empty());
        assert!(cli_php_prelude_args(Role::Composer).is_empty());
    }

    #[test]
    fn detects_each_role_by_basename() {
        assert_eq!(
            role_from_argv0(&OsString::from("/proj/vendor/bougie/bin/php")),
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
        assert_eq!(
            role_from_argv0(&OsString::from("/proj/vendor/bougie/bin/unzip")),
            Some(Role::Unzip)
        );
        assert_eq!(
            role_from_argv0(&OsString::from("/usr/local/bin/bougie-run")),
            Some(Role::ScriptRun)
        );
        assert_eq!(role_from_argv0(&OsString::from("bougie-run")), Some(Role::ScriptRun));
    }

    #[test]
    fn ignores_bougie_basename() {
        assert_eq!(role_from_argv0(&OsString::from("/usr/bin/bougie")), None);
        assert_eq!(role_from_argv0(&OsString::from("bougie")), None);
    }

    /// Windows file names are case-insensitive and PHP's
    /// `ZipDownloader` surfaces the invocation as `unzip.EXE`
    /// (capitalised) on some PATH-lookup paths. Without case-
    /// insensitive `.exe` stripping the shim falls through to the
    /// global clap parser and `unzip -qq` errors with "the argument
    /// '--quiet' cannot be used multiple times".
    #[test]
    fn strips_dot_exe_case_insensitively() {
        // Stripping is the shim's job on every OS — there's no harm
        // in trimming `.exe` on a Unix invocation, and the cross-
        // platform test surface stays uniform.
        assert_eq!(role_from_argv0(&OsString::from("unzip.exe")), Some(Role::Unzip));
        assert_eq!(role_from_argv0(&OsString::from("unzip.EXE")), Some(Role::Unzip));
        assert_eq!(role_from_argv0(&OsString::from("php.Exe")), Some(Role::Php));
        // The backslash-path assertion only works on Windows, where
        // `Path::file_name` recognises `\` as a separator. On Unix
        // the whole string is one filename and no role matches.
        #[cfg(windows)]
        assert_eq!(
            role_from_argv0(&OsString::from("C:\\proj\\.bougie\\bin\\composer.EXE")),
            Some(Role::Composer)
        );
    }

    #[cfg(unix)]
    #[test]
    fn detects_bougied_role() {
        assert_eq!(role_from_argv0(&OsString::from("bougied")), Some(Role::Bougied));
        assert_eq!(
            role_from_argv0(&OsString::from("/usr/local/bin/bougied")),
            Some(Role::Bougied)
        );
    }

    #[cfg(unix)]
    #[test]
    fn detects_babysit_role() {
        assert_eq!(
            role_from_argv0(&OsString::from("bougie-babysit")),
            Some(Role::Babysit)
        );
        assert_eq!(
            role_from_argv0(&OsString::from("/usr/local/bin/bougie-babysit")),
            Some(Role::Babysit)
        );
    }

    #[test]
    fn locate_project_root_walks_four_parents() {
        let root = locate_project_root_inner(
            OsStr::new("/proj/vendor/bougie/bin/php"),
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
        std::fs::create_dir_all(proj.path().join("vendor").join("bougie")).unwrap();
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
